//! Parse, authorize (by caller role), plan, and execute GQL against [`GraphStore`].

use crate::facade::GraphStore;
use crate::plan::{PlanMutationExecutor, PlanQueryExecutor, PlanQueryResult};
use gleaph_auth::Role;
use gleaph_gql::Value;
use gleaph_gql::ast::{GqlProgram, Statement};
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql_planner::build_statement_plan;
use std::collections::BTreeMap;

#[derive(Debug)]
pub enum GqlRunError {
    Parse(String),
    Plan(String),
    Auth(String),
    Query(crate::plan::PlanQueryError),
    Mutation(crate::plan::PlanMutationError),
}

impl std::fmt::Display for GqlRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(s) => write!(f, "parse error: {s}"),
            Self::Plan(s) => write!(f, "plan error: {s}"),
            Self::Auth(s) => write!(f, "authorization error: {s}"),
            Self::Query(e) => write!(f, "{e}"),
            Self::Mutation(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for GqlRunError {}

impl From<crate::plan::PlanQueryError> for GqlRunError {
    fn from(value: crate::plan::PlanQueryError) -> Self {
        Self::Query(value)
    }
}

impl From<crate::plan::PlanMutationError> for GqlRunError {
    fn from(value: crate::plan::PlanMutationError) -> Self {
        Self::Mutation(value)
    }
}

/// Ad-hoc GQL text (not prepared). Requires at least [`Role::Read`]; write path requires [`Role::Write`].
pub fn run_adhoc_gql(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
    caller_role: Role,
) -> Result<PlanQueryResult, GqlRunError> {
    if !caller_role.satisfies_at_least(Role::Read) {
        return Err(GqlRunError::Auth(
            "ad-hoc GQL requires Read role or higher".into(),
        ));
    }
    let program = parser::parse(gql).map_err(|e| GqlRunError::Parse(e.to_string()))?;
    let flags = classify_program(&program);
    if flags.requires_write_path() && !caller_role.satisfies_at_least(Role::Write) {
        return Err(GqlRunError::Auth(
            "this GQL program requires Write role or higher".into(),
        ));
    }

    let tx = program
        .transaction_activity
        .ok_or_else(|| GqlRunError::Parse("missing transaction".into()))?;
    let block = tx
        .body
        .ok_or_else(|| GqlRunError::Parse("missing statement block".into()))?;

    let mut last: PlanQueryResult = PlanQueryResult::default();
    for stmt in block.iter_statements() {
        if matches!(stmt, Statement::Session(_)) {
            continue;
        }
        let plan =
            build_statement_plan(stmt, None).map_err(|e| GqlRunError::Plan(e.to_string()))?;
        if plan.has_dml() {
            store.execute_plan_mutations(&plan)?;
        } else {
            last = store.execute_plan_query(&plan, parameters)?;
        }
    }
    Ok(last)
}

/// Run a prepared program loaded from the catalog on [`GraphStore`] (see [`GraphStore::prepared_query_register`]).
pub fn run_prepared_gql(
    store: GraphStore,
    program: &GqlProgram,
    parameters: &BTreeMap<String, Value>,
) -> Result<PlanQueryResult, GqlRunError> {
    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| GqlRunError::Parse("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| GqlRunError::Parse("missing statement block".into()))?;

    let mut last: PlanQueryResult = PlanQueryResult::default();
    for stmt in block.iter_statements() {
        if matches!(stmt, Statement::Session(_)) {
            continue;
        }
        let plan =
            build_statement_plan(stmt, None).map_err(|e| GqlRunError::Plan(e.to_string()))?;
        if plan.has_dml() {
            store.execute_plan_mutations(&plan)?;
        } else {
            last = store.execute_plan_query(&plan, parameters)?;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn adhoc_read_rejects_insert_for_read_role() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = run_adhoc_gql(store, "INSERT (n:Person {age: 1})", &params, Role::Read)
            .expect_err("expected auth error");
        assert!(err.to_string().contains("Write"), "unexpected error: {err}");
    }

    #[test]
    fn adhoc_write_allows_insert() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        run_adhoc_gql(store, "INSERT (n:TxTest {age: 1})", &params, Role::Write).expect("insert");
        let q = run_adhoc_gql(store, "MATCH (n:TxTest) RETURN n.age", &params, Role::Read)
            .expect("match");
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0].get("n.age"), Some(&Value::Int64(1)));
    }
}
