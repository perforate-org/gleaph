//! Semantic validation for GQL AST.
//!
//! This module performs post-parse validation checks that cannot be expressed
//! purely in the grammar. It validates variable scoping, structural constraints
//! on patterns and modification statements, and other semantic rules.

use crate::ast::*;
use crate::error::GqlError;
use rapidhash::RapidHashSet;

mod ddl;
mod dml;
mod expr;
mod graph_type;
mod procedure;
mod query_validation;
mod session;
mod session_graph;
mod transaction;

pub use session_graph::SessionGraphSeed;

use procedure::{
    validate_call_procedure, validate_inline_scope_vars, validate_yield_alias_uniqueness,
};
use query_validation::{composite_query_result_scopes, validate_composite_query};
use session::validate_session_command;
use transaction::validate_transaction_activity;

use ddl::{
    validate_create_graph, validate_create_graph_type, validate_create_schema, validate_drop_name,
};
use dml::{validate_delete, validate_insert, validate_remove_items, validate_set_items};

/// Result alias for validation.
type VResult = Result<(), GqlError>;

/// Validates a parsed [`GqlProgram`].
///
/// Returns `Ok(())` if the program passes all semantic checks, or a
/// [`GqlError::Validation`] describing the first violation found.
pub fn validate(program: &GqlProgram) -> VResult {
    validate_with_seed(program, None)
}

/// Validates `program` with optional router session graph seed (ADR 0011 §2).
///
/// When `seed` is `Some`, `CURRENT_GRAPH` / `HOME_GRAPH` references must resolve
/// against the supplied names; when `None`, those identifiers are treated as
/// opaque catalog names (library / test default).
pub fn validate_with_seed(program: &GqlProgram, seed: Option<&SessionGraphSeed>) -> VResult {
    session_graph::with_validation_seed(seed, || validate_program(program))
}

fn validate_program(program: &GqlProgram) -> VResult {
    for cmd in &program.session_activity {
        validate_session_command(cmd)?;
    }

    if let Some(ref ta) = program.transaction_activity {
        validate_transaction_activity(ta)?;
    }
    Ok(())
}

/// Applies [`crate::name_limits`] to each segment of a catalog [`ObjectName`].
pub(super) fn validate_catalog_object_name(name: &ObjectName) -> VResult {
    for part in &name.parts {
        crate::name_limits::validate_catalog_name_part(part).map_err(|e| verr(&e.to_string()))?;
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════════════
// Statement dispatch
// ════════════════════════════════════════════════════════════════════════════════

fn validate_statement_with_scope(
    stmt: &Statement,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    match stmt {
        Statement::Query(cq) => validate_composite_query(cq, scope, graph_scope),

        // — DDL (§12) —
        Statement::CreateSchema(create) => validate_create_schema(create),
        Statement::DropSchema(drop) => validate_drop_name(&drop.name, "DROP SCHEMA"),
        Statement::CreateGraph(create) => validate_create_graph(create),
        Statement::DropGraph(drop) => validate_drop_name(&drop.name, "DROP GRAPH"),
        Statement::CreateGraphType(create) => validate_create_graph_type(create),
        Statement::DropGraphType(drop) => validate_drop_name(&drop.name, "DROP GRAPH TYPE"),

        // — DML (§13) —
        Statement::Insert(ins) => validate_insert(ins),
        Statement::Set(set) => validate_set_items(&set.items),
        Statement::Remove(rem) => validate_remove_items(&rem.items),
        Statement::Delete(del) => validate_delete(del),

        // — Session (§7) — already validated at the program level.
        Statement::Session(_) => Ok(()),
    }
}

fn statement_result_scopes(
    stmt: &Statement,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> Result<(RapidHashSet<String>, RapidHashSet<String>), GqlError> {
    match stmt {
        Statement::Query(cq) => composite_query_result_scopes(cq, scope, graph_scope),
        _ => Ok((RapidHashSet::default(), RapidHashSet::default())),
    }
}

// ════════════════════════════════════════════════════════════════════════════════
// Helpers
// ════════════════════════════════════════════════════════════════════════════════

fn verr(msg: &str) -> GqlError {
    GqlError::Validation(msg.to_string())
}

#[cfg(test)]
mod tests;
