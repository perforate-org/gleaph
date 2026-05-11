//! Static type inference and warning-mode type checking for GQL programs.
//!
//! Emits **warnings** (not errors) for provably wrong type combinations.
//! `Unknown` suppresses all warnings (open-world assumption).

pub mod constraint;
mod diagnostics;
mod env;
mod graph_type_schema;
mod infer;
mod narrowing;
mod pattern;
mod phase_b;
pub mod schema;
pub mod types;

pub use constraint::{ConstraintSet, TypeVarId, TypedConstraint};
pub use diagnostics::{
    BindingKind, DML001_UNSUPPORTED_SET_REPLACE, DML002_TARGET_VALUE, DML003_TARGET_PATH,
    DML004_TARGET_UNKNOWN, DML005_INSERT_EDGE_DIRECTION, DML006_MATCH_EDGE_DIRECTION,
    DiagnosticSeverity, DmlDiagnostic, DmlDiagnosticSeverity, TypeDiagnostic,
    dml_diagnostic_from_warning, dml_diagnostic_severity, dml_target_path_message,
    dml_target_unknown_message, dml_target_value_message, dml_unsupported_set_replace_message,
    type_diagnostic_from_warning,
};
pub use env::{TypeWarning, WarningKind, WarningProvenance};
pub use graph_type_schema::GraphTypePropertySchema;
pub use phase_b::{
    infer_linear_query_binding_kinds, infer_linear_query_binding_kinds_and_warnings,
    infer_linear_query_binding_kinds_and_warnings_with_schema,
    infer_linear_query_binding_kinds_and_warnings_with_seed,
    infer_linear_query_binding_kinds_with_schema, infer_linear_query_binding_kinds_with_seed,
    infer_statement_block_binding_kinds, infer_statement_block_binding_kinds_with_schema,
    type_check_phase_b,
};
pub use schema::{NoSchema, ProcedureSignature, PropertySchema};
pub use types::{EdgeTypeInfo, NodeTypeInfo, PathTypeInfo, Type};

use crate::ast::*;
use crate::error::GqlError;
use crate::token::Span;
use rapidhash::RapidHashMap;
use std::collections::BTreeMap;

use env::TypeEnv;
use infer::{
    check_arithmetic, check_boolean_context, check_comparison, check_function_args,
    check_null_test, check_string_predicate, check_unary_op, infer_expr,
};
use narrowing::{apply_narrowing, extract_narrowing_facts};
use pattern::{
    build_env_from_graph_pattern, check_graph_pattern_schema_edge_direction,
    check_insert_path_schema_edge_direction,
};

/// Run static type checking on a parsed program. Returns warnings.
pub fn type_check(program: &GqlProgram) -> Vec<TypeWarning> {
    type_check_with_schema(program, &NoSchema)
}

/// Run static type checking with schema awareness.
pub fn type_check_with_schema(
    program: &GqlProgram,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    let mut env = TypeEnv::new(schema);
    check_program(&mut env, program);
    env.warnings
}

pub fn type_check_statement(stmt: &Statement) -> Vec<TypeWarning> {
    type_check_statement_with_schema(stmt, &NoSchema)
}

pub fn type_check_statement_with_schema(
    stmt: &Statement,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    let mut env = TypeEnv::new(schema);
    check_statement(&mut env, stmt);
    env.warnings
}

pub fn type_check_statement_block(block: &StatementBlock) -> Vec<TypeWarning> {
    type_check_statement_block_with_schema(block, &NoSchema)
}

pub fn type_check_statement_block_with_schema(
    block: &StatementBlock,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    let mut env = TypeEnv::new(schema);
    check_statement_block(&mut env, block);
    env.warnings
}

pub fn type_check_linear_query(query: &LinearQueryStatement) -> Vec<TypeWarning> {
    type_check_linear_query_with_schema(query, &NoSchema)
}

pub fn type_check_linear_query_with_schema(
    query: &LinearQueryStatement,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    let mut env = TypeEnv::new(schema);
    check_linear_query(&mut env, query);
    env.warnings
}

pub fn type_check_composite_query(query: &CompositeQueryExpr) -> Vec<TypeWarning> {
    type_check_composite_query_with_schema(query, &NoSchema)
}

pub fn type_check_composite_query_with_schema(
    query: &CompositeQueryExpr,
    schema: &dyn PropertySchema,
) -> Vec<TypeWarning> {
    let mut env = TypeEnv::new(schema);
    check_composite_query(&mut env, query);
    env.warnings
}

/// Same type-check traversal as [`type_check_composite_query_with_schema`], plus one [`BindingKind`]
/// map per composite branch (left query, then each right operand in order).
pub fn infer_composite_query_binding_kinds_and_warnings_with_schema(
    cq: &CompositeQueryExpr,
    schema: &dyn PropertySchema,
) -> (Vec<BTreeMap<String, BindingKind>>, Vec<TypeWarning>) {
    let mut env = TypeEnv::new(schema);
    let mut kinds_acc = Some(Vec::new());
    walk_composite_query_for_type_check(&mut env, cq, &mut kinds_acc);
    (
        kinds_acc.unwrap_or_default(),
        env.warnings,
    )
}

/// Strict-mode: returns `GqlError::TypeError` on first warning.
pub fn type_check_strict(
    program: &GqlProgram,
    schema: &dyn PropertySchema,
) -> Result<(), GqlError> {
    let warnings = type_check_with_schema(program, schema);
    if let Some(first) = warnings.first() {
        Err(GqlError::TypeError(first.message.clone()))
    } else {
        Ok(())
    }
}

fn binding_kind_from_type(ty: &Type) -> BindingKind {
    match ty {
        Type::Node(_) => BindingKind::Node,
        Type::Edge(_) => BindingKind::Edge,
        Type::Path(_) => BindingKind::Path,
        Type::Unknown => BindingKind::Unknown,
        _ => BindingKind::Value,
    }
}

fn dml_target_code_for_type(ty: &Type) -> Option<&'static str> {
    match binding_kind_from_type(ty) {
        BindingKind::Value => Some(DML002_TARGET_VALUE),
        BindingKind::Path => Some(DML003_TARGET_PATH),
        BindingKind::Unknown => Some(DML004_TARGET_UNKNOWN),
        BindingKind::Node | BindingKind::Edge => None,
    }
}

fn check_dml_target(env: &mut TypeEnv<'_>, op_name: &str, variable: &str, span: Span) {
    let ty = env.get(variable);
    match binding_kind_from_type(&ty) {
        BindingKind::Node | BindingKind::Edge => {}
        BindingKind::Path => env.warn_at_with_code(
            WarningKind::DmlTargetMismatch,
            DML003_TARGET_PATH,
            dml_target_path_message(op_name, Some(variable)),
            span,
        ),
        BindingKind::Value => env.warn_at_with_code(
            WarningKind::DmlTargetMismatch,
            DML002_TARGET_VALUE,
            dml_target_value_message(op_name, Some(variable)),
            span,
        ),
        BindingKind::Unknown => env.warn_at_with_code(
            WarningKind::DmlTargetMismatch,
            DML004_TARGET_UNKNOWN,
            dml_target_unknown_message(op_name, Some(variable)),
            span,
        ),
    }
}

// ── Program / statement walk ──

fn check_program(env: &mut TypeEnv<'_>, program: &GqlProgram) {
    if let Some(ref ta) = program.transaction_activity
        && let Some(ref body) = ta.body
    {
        check_statement_block(env, body);
    }
}

fn check_statement_block(env: &mut TypeEnv<'_>, block: &StatementBlock) {
    check_statement(env, &block.first);

    // Propagate types across NEXT boundaries.
    let mut prev_return_types = infer_statement_return_types(env, &block.first);

    for next in &block.next {
        // If there are yield items, project only those columns into the next scope.
        // Otherwise, all columns from the previous RETURN carry over.
        let mut next_scope = RapidHashMap::default();
        if let Some(ref yield_items) = next.yield_items {
            for yi in yield_items {
                let binding_name = yi.alias.as_deref().unwrap_or(&yi.name);
                let ty = prev_return_types
                    .iter()
                    .find(|(name, _)| name == &yi.name)
                    .map(|(_, t)| t.clone())
                    .unwrap_or(Type::Unknown);
                next_scope.insert(binding_name.to_string(), ty);
            }
        } else {
            // No explicit yield → all columns pass through.
            for (name, ty) in &prev_return_types {
                next_scope.insert(name.clone(), ty.clone());
            }
        }
        env.replace_scope(next_scope);

        check_statement(env, &next.statement);
        prev_return_types = infer_statement_return_types(env, &next.statement);
    }
}

/// Infer the output column types from a statement's RETURN clause.
/// Returns `Vec<(column_name, Type)>`.
fn infer_statement_return_types(env: &TypeEnv<'_>, stmt: &Statement) -> Vec<(String, Type)> {
    match stmt {
        Statement::Query(cq) => infer_composite_query_return_types(env, cq),
        _ => Vec::new(),
    }
}

fn infer_composite_query_return_types(
    env: &TypeEnv<'_>,
    cq: &CompositeQueryExpr,
) -> Vec<(String, Type)> {
    infer_linear_query_return_types(env, &cq.left)
}

fn infer_linear_query_return_types(
    env: &TypeEnv<'_>,
    lq: &LinearQueryStatement,
) -> Vec<(String, Type)> {
    match lq.result.as_ref() {
        Some(ResultStatement::Return(ret)) => infer_return_types(env, ret),
        _ => Vec::new(),
    }
}

fn infer_return_types(env: &TypeEnv<'_>, ret: &ReturnStatement) -> Vec<(String, Type)> {
    match &ret.body {
        ReturnBody::Star => {
            // RETURN * — all current bindings carry over.
            env.bindings
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        }
        #[cfg(feature = "cypher")]
        ReturnBody::NoBindings => Vec::new(),
        ReturnBody::Items { items, .. } => items
            .iter()
            .map(|item| {
                let name = item
                    .alias
                    .clone()
                    .or_else(|| match &item.expr.kind {
                        ExprKind::Variable(v) => Some(v.clone()),
                        ExprKind::PropertyAccess { property, .. } => Some(property.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "<expr>".to_string());
                let ty = infer_expr(env, &item.expr);
                (name, ty)
            })
            .collect(),
    }
}

fn check_statement(env: &mut TypeEnv<'_>, stmt: &Statement) {
    match stmt {
        Statement::Query(cq) => check_composite_query(env, cq),
        Statement::Insert(ins) => check_insert(env, ins),
        Statement::Set(s) => check_set(env, s),
        Statement::Remove(r) => check_remove(env, r),
        Statement::Delete(d) => check_delete(env, d),
        Statement::CreateSchema(_)
        | Statement::DropSchema(_)
        | Statement::CreateGraph(_)
        | Statement::DropGraph(_)
        | Statement::CreateGraphType(_)
        | Statement::DropGraphType(_)
        | Statement::Session(_) => {}
    }
}

fn walk_composite_query_for_type_check(
    env: &mut TypeEnv<'_>,
    cq: &CompositeQueryExpr,
    kinds_acc: &mut Option<Vec<BTreeMap<String, BindingKind>>>,
) {
    check_linear_query(env, &cq.left);
    if let Some(kinds) = kinds_acc {
        kinds.push(phase_b::binding_kinds_from_env(env));
    }
    if cq.rest.is_empty() {
        return;
    }
    let left_types = infer_return_column_types(env, &cq.left);
    for (op, lq) in &cq.rest {
        let mut branch_env = env.fork();
        check_linear_query(&mut branch_env, lq);
        if let Some(kinds) = kinds_acc {
            kinds.push(phase_b::binding_kinds_from_env(&branch_env));
        }
        let right_types = infer_return_column_types(&branch_env, lq);
        env.warnings.extend(branch_env.warnings);
        if left_types.len() != right_types.len() {
            env.warn_at(
                WarningKind::SetOpColumnMismatch,
                format!(
                    "{} branches have different column counts: {} vs {}",
                    set_op_name(op),
                    left_types.len(),
                    right_types.len()
                ),
                cq.span,
            );
            continue;
        }
        for (i, (lt, rt)) in left_types.iter().zip(right_types.iter()).enumerate() {
            if !set_op_types_compatible(lt, rt) {
                env.warn_at(
                    WarningKind::SetOpColumnMismatch,
                    format!(
                        "{} column {} has incompatible types: {lt:?} vs {rt:?}",
                        set_op_name(op),
                        i + 1
                    ),
                    cq.span,
                );
            }
        }
    }
}

fn check_composite_query(env: &mut TypeEnv<'_>, cq: &CompositeQueryExpr) {
    walk_composite_query_for_type_check(env, cq, &mut None);
}

/// Extract inferred types for each RETURN column in a linear query.
fn infer_return_column_types(env: &TypeEnv<'_>, lq: &LinearQueryStatement) -> Vec<Type> {
    match &lq.result {
        Some(ResultStatement::Return(ret)) => match &ret.body {
            ReturnBody::Items { items, .. } => items
                .iter()
                .map(|item| infer_expr(env, &item.expr))
                .collect(),
            _ => vec![],
        },
        Some(ResultStatement::Select(sel)) => match &sel.body {
            SelectBody::Items { items, .. } => items
                .iter()
                .map(|item| infer_expr(env, &item.expr))
                .collect(),
            _ => vec![],
        },
        _ => vec![],
    }
}

fn set_op_name(op: &SetOp) -> &'static str {
    match op {
        SetOp::Union | SetOp::UnionAll | SetOp::UnionDistinct => "UNION",
        SetOp::Except | SetOp::ExceptAll | SetOp::ExceptDistinct => "EXCEPT",
        SetOp::Intersect | SetOp::IntersectAll | SetOp::IntersectDistinct => "INTERSECT",
        SetOp::Otherwise => "OTHERWISE",
    }
}

/// Check if two types are compatible for set operations.
/// More lenient than strict equality: allows numeric promotion and Unknown.
fn set_op_types_compatible(a: &Type, b: &Type) -> bool {
    use types::*;
    let a = unwrap_nonnull(a);
    let b = unwrap_nonnull(b);
    if is_unknown(a) || is_unknown(b) {
        return true;
    }
    match (a, b) {
        (Type::Scalar(va), Type::Scalar(vb)) => {
            std::mem::discriminant(va) == std::mem::discriminant(vb)
                || (is_numeric_vt(va) && is_numeric_vt(vb))
        }
        (Type::Node(_), Type::Node(_))
        | (Type::Edge(_), Type::Edge(_))
        | (Type::Path(_), Type::Path(_)) => true,
        (Type::TypedList(_), Type::TypedList(_)) => true,
        _ => false,
    }
}

fn check_linear_query(env: &mut TypeEnv<'_>, lq: &LinearQueryStatement) {
    for part in &lq.parts {
        check_simple_query(env, part);
    }
    if let Some(ref result) = lq.result {
        check_result(env, result);
    }
}

fn check_simple_query(env: &mut TypeEnv<'_>, sq: &SimpleQueryStatement) {
    match sq {
        SimpleQueryStatement::Match(m) => check_match(env, m),
        SimpleQueryStatement::Filter(f) => {
            check_boolean_context(env, &f.condition);
            check_with_incremental_narrowing(env, &f.condition);
        }
        SimpleQueryStatement::Let(l) => {
            for binding in &l.bindings {
                let ty = infer_expr(env, &binding.value);
                check_expr_constraints(env, &binding.value);
                env.bind(binding.variable.clone(), ty);
            }
        }
        SimpleQueryStatement::For(f) => {
            check_expr_constraints(env, &f.list);
            // Infer element type from list type.
            let list_ty = infer_expr(env, &f.list);
            let elem_ty = match &list_ty {
                Type::TypedList(inner) => (**inner).clone(),
                _ => Type::Unknown,
            };
            env.bind(f.variable.clone(), elem_ty);
            if let Some(ref ord) = f.ordinality {
                env.bind(
                    ord.variable.clone(),
                    Type::Scalar(ValueType::Int64 {
                        keyword: Keyword::new("INT64"),
                    }),
                );
            }
        }
        SimpleQueryStatement::CallProcedure(cp) => {
            check_call_procedure(env, cp);
        }
        SimpleQueryStatement::InlineProcedureCall(ipc) => {
            check_composite_query(env, &ipc.body);
        }
        SimpleQueryStatement::Focused { body, .. } => {
            if let Some(inner) = body {
                check_simple_query(env, inner);
            }
        }
        SimpleQueryStatement::Insert(ins) => check_insert(env, ins),
        SimpleQueryStatement::Set(s) => check_set(env, s),
        SimpleQueryStatement::Remove(r) => check_remove(env, r),
        SimpleQueryStatement::Delete(d) => check_delete(env, d),
        SimpleQueryStatement::OrderBy(ob) => {
            for item in &ob.items {
                check_expr_constraints(env, &item.expr);
            }
        }
        SimpleQueryStatement::Limit(lim) => {
            check_expr_constraints(env, &lim.count);
            let ty = infer_expr(env, &lim.count);
            if !types::is_unknown(&ty)
                && !types::is_null(&ty)
                && !types::is_never(&ty)
                && !types::is_numeric(types::unwrap_nonnull(&ty))
            {
                env.warn_at(
                    WarningKind::NonNumericLimitOffset,
                    format!("LIMIT expects a numeric expression, got {ty:?}"),
                    lim.count.span,
                );
            }
        }
        SimpleQueryStatement::Offset(off) => {
            check_expr_constraints(env, &off.count);
            let ty = infer_expr(env, &off.count);
            if !types::is_unknown(&ty)
                && !types::is_null(&ty)
                && !types::is_never(&ty)
                && !types::is_numeric(types::unwrap_nonnull(&ty))
            {
                env.warn_at(
                    WarningKind::NonNumericLimitOffset,
                    format!("OFFSET expects a numeric expression, got {ty:?}"),
                    off.count.span,
                );
            }
        }
    }
}

fn check_match(env: &mut TypeEnv<'_>, m: &MatchStatement) {
    build_env_from_graph_pattern(env, &m.pattern, m.optional);
    check_graph_pattern_schema_edge_direction(env, &m.pattern);
    // Check WHERE clauses embedded inside node/edge patterns.
    check_pattern_internal_wheres(env, &m.pattern);
    if let Some(ref where_expr) = m.pattern.where_clause {
        check_boolean_context(env, where_expr);
        check_with_incremental_narrowing(env, where_expr);
    }
}

/// Walk a graph pattern and check WHERE clauses embedded inside node/edge patterns.
fn check_pattern_internal_wheres(env: &mut TypeEnv<'_>, gp: &GraphPattern) {
    for path in &gp.paths {
        check_path_expr_internal_wheres(env, &path.expr);
    }
}

fn check_path_expr_internal_wheres(env: &mut TypeEnv<'_>, expr: &PathPatternExpr) {
    match expr {
        PathPatternExpr::Term(term) => {
            for factor in &term.factors {
                check_path_primary_internal_wheres(env, &factor.primary);
            }
        }
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                for factor in &term.factors {
                    check_path_primary_internal_wheres(env, &factor.primary);
                }
            }
        }
    }
}

fn check_path_primary_internal_wheres(env: &mut TypeEnv<'_>, primary: &PathPrimary) {
    match primary {
        PathPrimary::Node(node) => {
            if let Some(ref w) = node.where_clause {
                check_boolean_context(env, w);
                check_with_incremental_narrowing(env, w);
            }
        }
        PathPrimary::Edge(edge) => {
            if let Some(ref w) = edge.where_clause {
                check_boolean_context(env, w);
                check_with_incremental_narrowing(env, w);
            }
        }
        PathPrimary::Parenthesized {
            expr, where_clause, ..
        } => {
            check_path_expr_internal_wheres(env, expr);
            if let Some(w) = where_clause {
                check_boolean_context(env, w);
                check_with_incremental_narrowing(env, w);
            }
        }
        PathPrimary::Simplified(_) => {}
    }
}

/// Process a WHERE/FILTER expression by splitting top-level AND conjuncts
/// and applying narrowing incrementally between each one.
///
/// For `WHERE a AND b AND c`, this:
/// 1. Extracts narrowing from `a`, applies it to env
/// 2. Checks `b` (now with `a`'s narrowing in effect), extracts & applies `b`'s narrowing
/// 3. Checks `c` (now with both `a` and `b`'s narrowing in effect), etc.
fn check_with_incremental_narrowing(env: &mut TypeEnv<'_>, expr: &Expr) {
    let conjuncts = flatten_and_conjuncts(expr);
    for conjunct in &conjuncts {
        check_expr_constraints(env, conjunct);
        let facts = extract_narrowing_facts(conjunct);
        apply_narrowing(env, &facts);
    }
}

/// Flatten top-level AND expressions into a list of conjuncts.
/// `a AND b AND c` → `[a, b, c]`
fn flatten_and_conjuncts(expr: &Expr) -> Vec<&Expr> {
    let mut out = Vec::new();
    collect_and_conjuncts(expr, &mut out);
    out
}

fn collect_and_conjuncts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    match &expr.kind {
        ExprKind::And(left, right) => {
            collect_and_conjuncts(left, out);
            collect_and_conjuncts(right, out);
        }
        _ => out.push(expr),
    }
}

fn check_result(env: &mut TypeEnv<'_>, result: &ResultStatement) {
    match result {
        ResultStatement::Return(ret) => check_return(env, ret),
        ResultStatement::Select(sel) => check_select(env, sel),
        ResultStatement::Finish => {}
    }
}

fn check_return(env: &mut TypeEnv<'_>, ret: &ReturnStatement) {
    match &ret.body {
        ReturnBody::Star => {}
        #[cfg(feature = "cypher")]
        ReturnBody::NoBindings => {}
        ReturnBody::Items {
            items,
            group_by,
            having,
            order_by,
            ..
        } => {
            for item in items {
                check_expr_constraints(env, &item.expr);
            }
            if let Some(h) = having {
                check_boolean_context(env, h);
            }
            check_aggregation_boundary(env, items, group_by.as_ref());
            // Bind RETURN aliases so ORDER BY can reference them.
            for item in items {
                if let Some(ref alias) = item.alias {
                    let ty = infer_expr(env, &item.expr);
                    env.bind(alias.clone(), ty);
                }
            }
            if let Some(ob) = order_by {
                for sort_item in &ob.items {
                    check_expr_constraints(env, &sort_item.expr);
                }
            }
        }
    }
}

fn check_select(env: &mut TypeEnv<'_>, sel: &SelectStatement) {
    // Check source match statements.
    if let Some(ref source) = sel.source {
        match source {
            SelectSource::GraphMatchList(list) => {
                for gm in list {
                    check_match(env, &gm.match_statement);
                }
            }
            SelectSource::QuerySpecification(spec) => match spec {
                SelectQuerySpecification::Nested(cq) => check_composite_query(env, cq),
                SelectQuerySpecification::GraphNested { query, .. } => {
                    check_composite_query(env, query)
                }
            },
        }
    }
    match &sel.body {
        SelectBody::Star { having, .. } => {
            if let Some(h) = having {
                check_boolean_context(env, h);
            }
        }
        SelectBody::Items {
            items,
            group_by,
            having,
            order_by,
            ..
        } => {
            for item in items {
                check_expr_constraints(env, &item.expr);
            }
            if let Some(h) = having {
                check_boolean_context(env, h);
            }
            check_aggregation_boundary(env, items, group_by.as_ref());
            // Bind SELECT aliases for ORDER BY.
            for item in items {
                if let Some(ref alias) = item.alias {
                    let ty = infer_expr(env, &item.expr);
                    env.bind(alias.clone(), ty);
                }
            }
            if let Some(ob) = order_by {
                for sort_item in &ob.items {
                    check_expr_constraints(env, &sort_item.expr);
                }
            }
        }
    }
}

fn check_insert(env: &mut TypeEnv<'_>, ins: &InsertStatement) {
    for path_pattern in &ins.patterns {
        check_insert_path_schema_edge_direction(env, path_pattern);
        for element in &path_pattern.elements {
            match element {
                InsertElement::Node(node) => {
                    let schema_props = if !node.labels.is_empty() {
                        env.schema.node_property_types(&node.labels)
                    } else {
                        Vec::new()
                    };
                    for prop in &node.properties {
                        check_expr_constraints(env, &prop.value);
                        check_property_assignment(env, &schema_props, &prop.name, &prop.value);
                    }
                    check_required_properties(env, &schema_props, &node.properties, node.span);
                }
                InsertElement::Edge(edge) => {
                    let schema_props = if let Some(label) = edge.labels.first() {
                        env.schema.edge_property_types(label)
                    } else {
                        Vec::new()
                    };
                    for prop in &edge.properties {
                        check_expr_constraints(env, &prop.value);
                        check_property_assignment(env, &schema_props, &prop.name, &prop.value);
                    }
                    check_required_properties(env, &schema_props, &edge.properties, edge.span);
                }
            }
        }
    }
}

fn check_set(env: &mut TypeEnv<'_>, s: &SetStatement) {
    for item in &s.items {
        match item {
            SetItem::Property {
                span,
                variable,
                property,
                value,
                ..
            } => {
                check_expr_constraints(env, value);
                check_dml_target(env, "SET property", variable, *span);
                // Look up the target variable's schema properties.
                let schema_props = match env.get(variable) {
                    Type::Node(ref info) => {
                        if !info.properties.is_empty() {
                            info.properties.clone()
                        } else if info.has_labels() {
                            env.schema.node_property_types(info.primary_labels())
                        } else {
                            Vec::new()
                        }
                    }
                    Type::Edge(ref info) => {
                        if !info.properties.is_empty() {
                            info.properties.clone()
                        } else if let Some(ref label) = info.label {
                            env.schema.edge_property_types(label)
                        } else {
                            Vec::new()
                        }
                    }
                    _ => Vec::new(),
                };
                check_property_assignment(env, &schema_props, property, value);
            }
            SetItem::AllProperties {
                span,
                variable,
                value,
            } => {
                check_expr_constraints(env, value);
                check_dml_target(env, "SET", variable, *span);
                env.warn_at_with_code(
                    WarningKind::UnsupportedDml,
                    DML001_UNSUPPORTED_SET_REPLACE,
                    dml_unsupported_set_replace_message(variable),
                    *span,
                );
            }
            SetItem::Label { span, variable, .. } => {
                check_dml_target(env, "SET label", variable, *span);
            }
        }
    }
}

fn check_remove(env: &mut TypeEnv<'_>, r: &RemoveStatement) {
    for item in &r.items {
        match item {
            RemoveItem::Property { span, variable, .. } => {
                check_dml_target(env, "REMOVE property", variable, *span);
            }
            RemoveItem::Label { span, variable, .. } => {
                check_dml_target(env, "REMOVE label", variable, *span);
            }
        }
    }
}

fn check_delete(env: &mut TypeEnv<'_>, d: &DeleteStatement) {
    for item in &d.items {
        check_expr_constraints(env, item);
        let ty = infer_expr(env, item);
        let target_variable = match &item.kind {
            ExprKind::Variable(variable) => Some(variable.as_str()),
            _ => None,
        };
        match &ty {
            Type::Node(_) | Type::Edge(_) | Type::Unknown => {}
            _ => env.warn_at_with_code(
                WarningKind::DmlTargetMismatch,
                dml_target_code_for_type(&ty).unwrap_or(DML002_TARGET_VALUE),
                match binding_kind_from_type(&ty) {
                    BindingKind::Path => dml_target_path_message("DELETE", target_variable),
                    _ => dml_target_value_message("DELETE", target_variable),
                },
                item.span,
            ),
        }
    }
}

// ── Property assignment checking ──

/// Check that a value assigned to a named property is compatible with the schema type.
fn check_property_assignment(
    env: &mut TypeEnv<'_>,
    schema_props: &[(String, ValueType, bool)],
    prop_name: &str,
    value_expr: &Expr,
) {
    let Some((_, expected_vt, _)) = schema_props.iter().find(|(name, _, _)| name == prop_name)
    else {
        return; // Property not in schema → open-world, skip.
    };
    let actual_ty = infer_expr(env, value_expr);
    if types::is_unknown(&actual_ty) || types::is_never(&actual_ty) || types::is_null(&actual_ty) {
        return;
    }
    let expected_ty = Type::Scalar(expected_vt.clone());
    if !property_types_compatible(&actual_ty, &expected_ty) {
        env.warn_at(
            WarningKind::PropertyTypeMismatch,
            format!("property `{prop_name}` expects {expected_vt:?}, got {actual_ty:?}",),
            value_expr.span,
        );
    }
}

/// Check if an actual type is compatible with an expected property type.
fn property_types_compatible(actual: &Type, expected: &Type) -> bool {
    use types::*;
    let actual = unwrap_nonnull(actual);
    let expected = unwrap_nonnull(expected);
    match (actual, expected) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Scalar(a), Type::Scalar(b)) => {
            // Same discriminant, or both numeric (promotion allowed).
            std::mem::discriminant(a) == std::mem::discriminant(b)
                || (is_numeric_vt(a) && is_numeric_vt(b))
        }
        (Type::TypedList(_), Type::TypedList(_)) => true,
        _ => false,
    }
}

// ── CallProcedure checking ──

fn check_call_procedure(env: &mut TypeEnv<'_>, cp: &CallProcedureStatement) {
    // Check argument expressions.
    for arg in &cp.args {
        check_expr_constraints(env, arg);
    }
    // Look up procedure signature from schema.
    let proc_name = cp.name.parts.join(".");
    if let Some(sig) = env.schema.procedure_signature(&proc_name) {
        // Check argument count.
        if cp.args.len() != sig.params.len() {
            env.warn_at(
                WarningKind::FunctionArgMismatch,
                format!(
                    "procedure `{proc_name}` expects {} argument(s), got {}",
                    sig.params.len(),
                    cp.args.len()
                ),
                cp.span,
            );
        }
        // Check argument types.
        for (arg, (param_name, param_type)) in cp.args.iter().zip(sig.params.iter()) {
            let arg_ty = infer_expr(env, arg);
            if types::is_unknown(&arg_ty) || types::is_null(&arg_ty) || types::is_never(&arg_ty) {
                continue;
            }
            let expected = Type::from_value_type(param_type);
            if !property_types_compatible(&arg_ty, &expected) {
                env.warn_at(
                    WarningKind::FunctionArgMismatch,
                    format!(
                        "procedure `{proc_name}` parameter `{param_name}` expects {param_type:?}, got {arg_ty:?}"
                    ),
                    arg.span,
                );
            }
        }
        // Bind YIELD columns.
        if let Some(ref yield_items) = cp.yield_items {
            for item in yield_items {
                let yield_type = sig
                    .yields
                    .iter()
                    .find(|(name, _)| name == &item.name)
                    .map(|(_, vt)| Type::from_value_type(vt))
                    .unwrap_or(Type::Unknown);
                let bind_name = item.alias.as_deref().unwrap_or(&item.name);
                env.bind(bind_name.to_string(), yield_type);
            }
        }
    } else {
        // No schema info — bind YIELD items as Unknown.
        if let Some(ref yield_items) = cp.yield_items {
            for item in yield_items {
                let bind_name = item.alias.as_deref().unwrap_or(&item.name);
                env.bind(bind_name.to_string(), Type::Unknown);
            }
        }
    }
}

// ── Required property checking ──

/// Check that all required (NOT NULL) properties are present in an INSERT.
fn check_required_properties(
    env: &mut TypeEnv<'_>,
    schema_props: &[(String, ValueType, bool)],
    provided: &[PropertySetting],
    span: Span,
) {
    let provided_names: Vec<&str> = provided.iter().map(|p| p.name.as_str()).collect();
    for (name, _, required) in schema_props {
        if *required && !provided_names.contains(&name.as_str()) {
            env.warn_at(
                WarningKind::MissingRequiredProperty,
                format!("required property `{name}` is missing from INSERT"),
                span,
            );
        }
    }
}

// ── Concat operand checking ──

/// Check that both operands of `||` are string-compatible.
fn check_concat_operands(env: &mut TypeEnv<'_>, left: &Expr, right: &Expr) {
    let lt = infer_expr(env, left);
    let rt = infer_expr(env, right);
    for (ty, expr) in [(&lt, left), (&rt, right)] {
        if types::is_unknown(ty) || types::is_null(ty) || types::is_never(ty) {
            continue;
        }
        let unwrapped = types::unwrap_nonnull(ty);
        if !matches!(unwrapped, Type::Scalar(vt) if types::is_string_vt(vt)) {
            env.warn_at(
                WarningKind::BinaryOpMismatch,
                format!("concatenation (||) expects string operands, got {ty:?}"),
                expr.span,
            );
            return; // One warning is enough.
        }
    }
}

// ── CASE branch type checking ──

/// Check that CASE/COALESCE branches return compatible types.
fn check_case_branch_types(env: &mut TypeEnv<'_>, branches: &[&Expr], span: Span) {
    use types::*;
    let types: Vec<Type> = branches
        .iter()
        .map(|e| infer_expr(env, e))
        .filter(|t| !is_unknown(t) && !is_null(t) && !is_never(t))
        .collect();
    if types.len() < 2 {
        return;
    }
    // Check all pairs for broad compatibility.
    let first = &types[0];
    for ty in &types[1..] {
        if !broadly_compatible(first, ty) {
            env.warn_at(
                WarningKind::CaseBranchTypeMismatch,
                format!("CASE branches return incompatible types: {first:?} vs {ty:?}"),
                span,
            );
            return;
        }
    }
}

/// Check if two types are broadly compatible (same category).
/// Numeric types are all compatible with each other (promotion).
/// String types are compatible. Temporal types of same kind are compatible.
fn broadly_compatible(a: &Type, b: &Type) -> bool {
    use types::*;
    let a = unwrap_nonnull(a);
    let b = unwrap_nonnull(b);
    match (a, b) {
        (Type::Unknown, _) | (_, Type::Unknown) => true,
        (Type::Scalar(va), Type::Scalar(vb)) => {
            std::mem::discriminant(va) == std::mem::discriminant(vb)
                || (is_numeric_vt(va) && is_numeric_vt(vb))
                || (is_string_vt(va) && is_string_vt(vb))
                || (is_temporal_vt(va) && is_temporal_vt(vb))
                || (is_duration_vt(va) && is_duration_vt(vb))
        }
        (Type::Node(_), Type::Node(_)) => true,
        (Type::Edge(_), Type::Edge(_)) => true,
        (Type::TypedList(_), Type::TypedList(_)) => true,
        (Type::Record(_), Type::Record(_)) => true,
        _ => false,
    }
}

// ── Expression constraint checking ──

/// Walk an expression and emit warnings for type mismatches.
fn check_expr_constraints(env: &mut TypeEnv<'_>, expr: &Expr) {
    match &expr.kind {
        ExprKind::BinaryOp { op, left, right } => {
            check_expr_constraints(env, left);
            check_expr_constraints(env, right);
            check_arithmetic(env, *op, left, right);
        }
        ExprKind::Compare { left, right, .. } => {
            check_expr_constraints(env, left);
            check_expr_constraints(env, right);
            check_comparison(env, left, right);
        }
        ExprKind::IsNull(inner) => {
            check_expr_constraints(env, inner);
            check_null_test(env, inner, false);
        }
        ExprKind::IsNotNull(inner) => {
            check_expr_constraints(env, inner);
            check_null_test(env, inner, true);
        }
        ExprKind::FunctionCall { name, args, .. } => {
            for arg in args {
                check_expr_constraints(env, arg);
            }
            let fn_name = name.parts.first().map(String::as_str).unwrap_or("");
            check_function_args(env, fn_name, args, expr.span);
        }
        ExprKind::Aggregate {
            expr: arg,
            expr2,
            filter,
            ..
        } => {
            if let Some(e) = arg {
                check_expr_constraints(env, e);
            }
            if let Some(e) = expr2 {
                check_expr_constraints(env, e);
            }
            if let Some(f) = filter {
                check_boolean_context(env, f);
            }
        }
        ExprKind::And(l, r) | ExprKind::Or(l, r) | ExprKind::Xor(l, r) => {
            check_expr_constraints(env, l);
            check_expr_constraints(env, r);
        }
        ExprKind::Not(inner) => check_expr_constraints(env, inner),
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            check_expr_constraints(env, operand);
            let mut branch_exprs: Vec<&Expr> = Vec::new();
            for wc in when_clauses {
                check_expr_constraints(env, &wc.condition);
                check_expr_constraints(env, &wc.result);
                branch_exprs.push(&wc.result);
            }
            if let Some(e) = else_clause {
                check_expr_constraints(env, e);
                branch_exprs.push(e);
            }
            check_case_branch_types(env, &branch_exprs, expr.span);
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            let mut branch_exprs: Vec<&Expr> = Vec::new();
            for wc in when_clauses {
                check_boolean_context(env, &wc.condition);
                check_expr_constraints(env, &wc.result);
                branch_exprs.push(&wc.result);
            }
            if let Some(e) = else_clause {
                check_expr_constraints(env, e);
                branch_exprs.push(e);
            }
            check_case_branch_types(env, &branch_exprs, expr.span);
        }
        ExprKind::Coalesce(exprs) => {
            for e in exprs {
                check_expr_constraints(env, e);
            }
        }
        ExprKind::UnaryOp { op, expr: inner } => {
            check_expr_constraints(env, inner);
            check_unary_op(env, *op, inner);
        }
        ExprKind::StringPredicate {
            expr: target,
            pattern,
            ..
        } => {
            check_expr_constraints(env, target);
            check_expr_constraints(env, pattern);
            check_string_predicate(env, target, pattern);
        }
        ExprKind::Concat(l, r) => {
            check_expr_constraints(env, l);
            check_expr_constraints(env, r);
            check_concat_operands(env, l, r);
        }
        ExprKind::Cast { expr: inner, .. } => {
            check_expr_constraints(env, inner);
        }
        ExprKind::PropertyAccess { expr: inner, .. } => {
            check_expr_constraints(env, inner);
        }
        ExprKind::NullIf(l, r) => {
            check_expr_constraints(env, l);
            check_expr_constraints(env, r);
        }
        ExprKind::Paren(inner) => {
            check_expr_constraints(env, inner);
        }
        ExprKind::ListLiteral(elems) | ExprKind::ListConstructor { items: elems, .. } => {
            for e in elems {
                check_expr_constraints(env, e);
            }
        }
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (_, v) in fields {
                check_expr_constraints(env, v);
            }
        }
        ExprKind::LetIn {
            bindings,
            expr: body,
        } => {
            let snapshot = env.snapshot_bindings();
            for b in bindings {
                check_expr_constraints(env, &b.value);
                let ty = infer_expr(env, &b.value);
                env.bind(b.variable.clone(), ty);
            }
            check_expr_constraints(env, body);
            env.restore_bindings(snapshot);
        }
        ExprKind::IsLabeled { expr: inner, .. }
        | ExprKind::IsTyped { expr: inner, .. }
        | ExprKind::IsDirected { expr: inner, .. }
        | ExprKind::IsNormalized { expr: inner, .. }
        | ExprKind::PropertyExists { expr: inner, .. }
        | ExprKind::IsTruth { expr: inner, .. } => {
            check_expr_constraints(env, inner);
        }
        ExprKind::IsSourceOf { node, edge, .. } | ExprKind::IsDestOf { node, edge, .. } => {
            check_expr_constraints(env, node);
            check_expr_constraints(env, edge);
        }
        ExprKind::AllDifferent(exprs) | ExprKind::Same(exprs) => {
            for e in exprs {
                check_expr_constraints(env, e);
            }
        }
        ExprKind::ExistsSubquery(cq) => {
            check_composite_query(env, cq);
        }
        ExprKind::ValueSubquery(cq) => {
            check_composite_query(env, cq);
        }
        // Dedicated string functions — validate arg is string.
        ExprKind::CharLength { expr: inner, .. } | ExprKind::ByteLength { expr: inner, .. } => {
            check_expr_constraints(env, inner);
            let ty = infer_expr(env, inner);
            if !types::is_unknown(&ty) && !types::is_null(&ty) && !types::is_never(&ty) {
                let unwrapped = types::unwrap_nonnull(&ty);
                if !matches!(unwrapped, Type::Scalar(vt) if types::is_string_vt(vt)) {
                    env.warn_at(
                        WarningKind::FunctionArgMismatch,
                        format!("string length function expects a string argument, got {ty:?}"),
                        inner.span,
                    );
                }
            }
        }
        // Dedicated numeric functions — validate arg is numeric.
        ExprKind::Abs(inner)
        | ExprKind::Floor(inner)
        | ExprKind::Ceil(inner)
        | ExprKind::Sqrt(inner)
        | ExprKind::Exp(inner)
        | ExprKind::Ln(inner)
        | ExprKind::Log10(inner)
        | ExprKind::Sin(inner)
        | ExprKind::Cos(inner)
        | ExprKind::Tan(inner)
        | ExprKind::Asin(inner)
        | ExprKind::Acos(inner)
        | ExprKind::Atan(inner)
        | ExprKind::Degrees(inner)
        | ExprKind::Radians(inner)
        | ExprKind::Cot(inner)
        | ExprKind::Sinh(inner)
        | ExprKind::Cosh(inner)
        | ExprKind::Tanh(inner) => {
            check_expr_constraints(env, inner);
            let ty = infer_expr(env, inner);
            if !types::is_unknown(&ty)
                && !types::is_null(&ty)
                && !types::is_never(&ty)
                && !types::is_numeric(types::unwrap_nonnull(&ty))
            {
                env.warn_at(
                    WarningKind::FunctionArgMismatch,
                    format!("numeric function expects a numeric argument, got {ty:?}"),
                    inner.span,
                );
            }
        }
        ExprKind::Mod(l, r) | ExprKind::Power(l, r) | ExprKind::Log(l, r) => {
            check_expr_constraints(env, l);
            check_expr_constraints(env, r);
        }
        ExprKind::Cardinality { expr: inner, .. } => {
            check_expr_constraints(env, inner);
            let ty = infer_expr(env, inner);
            if !types::is_unknown(&ty)
                && !types::is_null(&ty)
                && !types::is_never(&ty)
                && !matches!(types::unwrap_nonnull(&ty), Type::TypedList(_))
            {
                env.warn_at(
                    WarningKind::FunctionArgMismatch,
                    format!("cardinality/size expects a list argument, got {ty:?}"),
                    inner.span,
                );
            }
        }
        _ => {
            // Literal, Variable, Parameter, etc. — leaf nodes.
        }
    }
}

// ── Aggregation boundary checking ──

fn check_aggregation_boundary(
    env: &mut TypeEnv<'_>,
    items: &[ReturnItem],
    group_by: Option<&GroupByClause>,
) {
    let has_aggregate = items.iter().any(|i| expr_contains_aggregate(&i.expr));
    if !has_aggregate {
        return;
    }

    let Some(group_by) = group_by else {
        // Implicit grouping — the executor handles this.
        return;
    };

    for item in items {
        if expr_contains_aggregate(&item.expr) {
            continue;
        }
        if !is_grouping_key(&item.expr, &group_by.items) {
            let name = item
                .alias
                .as_deref()
                .or(match &item.expr.kind {
                    ExprKind::Variable(v) => Some(v.as_str()),
                    ExprKind::PropertyAccess { property, .. } => Some(property.as_str()),
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

fn is_grouping_key(expr: &Expr, group_keys: &[Expr]) -> bool {
    group_keys
        .iter()
        .any(|key| exprs_structurally_equal(expr, key))
}

fn exprs_structurally_equal(a: &Expr, b: &Expr) -> bool {
    match (&a.kind, &b.kind) {
        (ExprKind::Variable(va), ExprKind::Variable(vb)) => va == vb,
        (
            ExprKind::PropertyAccess {
                expr: ta,
                property: pa,
            },
            ExprKind::PropertyAccess {
                expr: tb,
                property: pb,
            },
        ) => pa == pb && exprs_structurally_equal(ta, tb),
        (ExprKind::Literal(la), ExprKind::Literal(lb)) => la == lb,
        (
            ExprKind::FunctionCall {
                name: na, args: aa, ..
            },
            ExprKind::FunctionCall {
                name: nb, args: ab, ..
            },
        ) => {
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

fn expr_contains_aggregate(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Aggregate { .. } => true,
        ExprKind::PropertyAccess { expr: target, .. }
        | ExprKind::UnaryOp { expr: target, .. }
        | ExprKind::Not(target)
        | ExprKind::IsNull(target)
        | ExprKind::IsNotNull(target)
        | ExprKind::PathLength(target)
        | ExprKind::Cast { expr: target, .. }
        | ExprKind::IsTruth { expr: target, .. }
        | ExprKind::IsLabeled { expr: target, .. }
        | ExprKind::IsDirected { expr: target, .. }
        | ExprKind::IsTyped { expr: target, .. }
        | ExprKind::Paren(target) => expr_contains_aggregate(target),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::Compare { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right)
        | ExprKind::IsSourceOf {
            node: left,
            edge: right,
            ..
        }
        | ExprKind::IsDestOf {
            node: left,
            edge: right,
            ..
        } => expr_contains_aggregate(left) || expr_contains_aggregate(right),
        ExprKind::FunctionCall { args, .. } => args.iter().any(expr_contains_aggregate),
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            expr_contains_aggregate(operand)
                || when_clauses.iter().any(|wc| {
                    expr_contains_aggregate(&wc.condition) || expr_contains_aggregate(&wc.result)
                })
                || else_clause
                    .as_ref()
                    .is_some_and(|e| expr_contains_aggregate(e))
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            when_clauses.iter().any(|wc| {
                expr_contains_aggregate(&wc.condition) || expr_contains_aggregate(&wc.result)
            }) || else_clause
                .as_ref()
                .is_some_and(|e| expr_contains_aggregate(e))
        }
        ExprKind::Coalesce(exprs)
        | ExprKind::ListLiteral(exprs)
        | ExprKind::ListConstructor { items: exprs, .. }
        | ExprKind::AllDifferent(exprs)
        | ExprKind::Same(exprs) => exprs.iter().any(expr_contains_aggregate),
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            fields.iter().any(|(_, e)| expr_contains_aggregate(e))
        }
        ExprKind::StringPredicate { expr, pattern, .. } => {
            expr_contains_aggregate(expr) || expr_contains_aggregate(pattern)
        }
        ExprKind::PropertyExists { expr: target, .. } => expr_contains_aggregate(target),
        ExprKind::LetIn {
            bindings,
            expr: body,
        } => {
            bindings.iter().any(|b| expr_contains_aggregate(&b.value))
                || expr_contains_aggregate(body)
        }
        ExprKind::ExistsSubquery(_) | ExprKind::ExistsPattern(_) | ExprKind::ValueSubquery(_) => {
            false // subquery aggregates are scoped
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::InList { expr, list, .. } => {
            expr_contains_aggregate(expr) || list.iter().any(expr_contains_aggregate)
        }
        _ => false,
    }
}
