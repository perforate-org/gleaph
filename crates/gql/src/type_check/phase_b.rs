use std::collections::BTreeMap;

use crate::ast::{GqlProgram, LinearQueryStatement, Statement, StatementBlock};

use super::env::TypeEnv;
use super::{
    BindingKind, EdgeTypeInfo, NoSchema, NodeTypeInfo, PathTypeInfo, PropertySchema, Type,
    check_linear_query, check_program, check_statement, constraint, infer_statement_return_types,
};

/// Run constraint-based (Phase B) type checking with schema awareness.
///
/// This collects typed constraints from all expressions, then solves them
/// in a separate pass. Warnings are prefixed with `[phase-b]` to distinguish
/// them from the direct-inference (Phase A) warnings.
pub fn type_check_phase_b(
    program: &GqlProgram,
    schema: &dyn PropertySchema,
) -> Vec<super::TypeWarning> {
    let mut env = TypeEnv::new(schema);
    check_program(&mut env, program);

    if let Some(ref ta) = program.transaction_activity
        && let Some(ref body) = ta.body
    {
        let mut cset = constraint::ConstraintSet::new();
        collect_constraints_from_block(&mut cset, &env, body);
        cset.solve(&mut env);
    }
    env.warnings
}

pub fn infer_linear_query_binding_kinds(
    query: &LinearQueryStatement,
) -> BTreeMap<String, BindingKind> {
    infer_linear_query_binding_kinds_with_schema(query, &NoSchema)
}

pub fn infer_linear_query_binding_kinds_with_schema(
    query: &LinearQueryStatement,
    schema: &dyn PropertySchema,
) -> BTreeMap<String, BindingKind> {
    infer_linear_query_binding_kinds_with_seed(query, schema, &BTreeMap::new())
}

pub fn infer_linear_query_binding_kinds_with_seed(
    query: &LinearQueryStatement,
    schema: &dyn PropertySchema,
    seed: &BTreeMap<String, BindingKind>,
) -> BTreeMap<String, BindingKind> {
    let mut env = TypeEnv::new(schema);
    seed_env_with_binding_kinds(&mut env, seed);
    check_linear_query(&mut env, query);
    binding_kinds_from_env(&env)
}

/// Like [`infer_linear_query_binding_kinds`], but also returns type warnings from the same
/// `check_linear_query` pass so callers can avoid running it twice.
pub fn infer_linear_query_binding_kinds_and_warnings(
    query: &LinearQueryStatement,
) -> (BTreeMap<String, BindingKind>, Vec<super::TypeWarning>) {
    infer_linear_query_binding_kinds_and_warnings_with_seed(query, &NoSchema, &BTreeMap::new())
}

pub fn infer_linear_query_binding_kinds_and_warnings_with_seed(
    query: &LinearQueryStatement,
    schema: &dyn PropertySchema,
    seed: &BTreeMap<String, BindingKind>,
) -> (BTreeMap<String, BindingKind>, Vec<super::TypeWarning>) {
    let mut env = TypeEnv::new(schema);
    seed_env_with_binding_kinds(&mut env, seed);
    check_linear_query(&mut env, query);
    (binding_kinds_from_env(&env), env.warnings)
}

pub fn infer_statement_block_binding_kinds(
    block: &StatementBlock,
) -> Vec<BTreeMap<String, BindingKind>> {
    infer_statement_block_binding_kinds_with_schema(block, &NoSchema)
}

pub fn infer_statement_block_binding_kinds_with_schema(
    block: &StatementBlock,
    schema: &dyn PropertySchema,
) -> Vec<BTreeMap<String, BindingKind>> {
    let mut env = TypeEnv::new(schema);
    let mut per_statement = Vec::with_capacity(1 + block.next.len());
    per_statement.push(binding_kinds_from_env(&env));

    check_statement(&mut env, &block.first);

    let mut prev_return_types = infer_statement_return_types(&env, &block.first);
    for next in &block.next {
        if let Some(ref yield_items) = next.yield_items {
            for yi in yield_items {
                let binding_name = yi.alias.as_deref().unwrap_or(&yi.name);
                let ty = prev_return_types
                    .iter()
                    .find(|(name, _)| name == &yi.name)
                    .map(|(_, t)| t.clone())
                    .unwrap_or(Type::Unknown);
                env.bind(binding_name.to_string(), ty);
            }
        } else {
            for (name, ty) in &prev_return_types {
                env.bind(name.clone(), ty.clone());
            }
        }

        per_statement.push(binding_kinds_from_env(&env));
        check_statement(&mut env, &next.statement);
        prev_return_types = infer_statement_return_types(&env, &next.statement);
    }

    per_statement
}

pub(super) fn binding_kinds_from_env(env: &TypeEnv<'_>) -> BTreeMap<String, BindingKind> {
    env.bindings
        .iter()
        .map(|(name, ty)| (name.clone(), super::binding_kind_from_type(ty)))
        .collect()
}

fn collect_constraints_from_block(
    cset: &mut constraint::ConstraintSet,
    env: &TypeEnv<'_>,
    block: &StatementBlock,
) {
    collect_constraints_from_statement(cset, env, &block.first);
    for next in &block.next {
        collect_constraints_from_statement(cset, env, &next.statement);
    }
}

fn collect_constraints_from_statement(
    cset: &mut constraint::ConstraintSet,
    env: &TypeEnv<'_>,
    stmt: &Statement,
) {
    if let Statement::Query(cq) = stmt {
        cset.collect_from_composite_query(env, cq);
    }
}

pub(super) fn seed_env_with_binding_kinds(
    env: &mut TypeEnv<'_>,
    seed: &BTreeMap<String, BindingKind>,
) {
    for (name, kind) in seed {
        env.bind(name.clone(), type_from_binding_kind(*kind));
    }
}

fn type_from_binding_kind(kind: BindingKind) -> Type {
    match kind {
        BindingKind::Node => Type::Node(NodeTypeInfo::from_labels(Vec::new())),
        BindingKind::Edge => Type::Edge(EdgeTypeInfo::from_label(None)),
        BindingKind::Path => Type::Path(PathTypeInfo::default()),
        BindingKind::Value | BindingKind::Unknown => Type::Unknown,
    }
}
