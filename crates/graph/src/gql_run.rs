//! Parse, authorize (by caller role), plan, and execute GQL against [`GraphStore`].

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::pending;
use crate::plan::{PlanMutationExecutor, PlanQueryResult, execute_plan_query};
use gleaph_auth::Role;
use gleaph_gql::Value;
use gleaph_gql::ast::Statement;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql_planner::build_statement_plan;
use gleaph_graph_prepared::PreparedQueryRecord;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlCanisterExecutionMode {
    /// Composite [`query`] entrypoint: program must not require a write path.
    CompositeQuery,
    /// [`update`] entrypoint: program must require a write path (mutations / DDL / CALL).
    Update,
}

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

fn enforce_execution_mode(
    mode: GqlCanisterExecutionMode,
    flags: gleaph_gql::program_modification::ProgramModificationFlags,
) -> Result<(), GqlRunError> {
    match mode {
        GqlCanisterExecutionMode::CompositeQuery if flags.requires_write_path() => {
            Err(GqlRunError::Plan(
                "program modifies data or catalog (or uses CALL); use gql_execute instead".into(),
            ))
        }
        GqlCanisterExecutionMode::Update if !flags.requires_write_path() => Err(GqlRunError::Plan(
            "program is read-only; use gql_query instead".into(),
        )),
        _ => Ok(()),
    }
}

/// Ad-hoc GQL text (not prepared). Caller supplies [`GqlCanisterExecutionMode`] matching the canister entrypoint.
pub async fn run_adhoc_gql(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
    caller_role: Role,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
) -> Result<PlanQueryResult, GqlRunError> {
    if !caller_role.satisfies_at_least(Role::Read) {
        return Err(GqlRunError::Auth(
            "GQL execution requires Read role or higher".into(),
        ));
    }
    let program = parser::parse(gql).map_err(|e| GqlRunError::Parse(e.to_string()))?;

    let flags = classify_program(&program);
    enforce_execution_mode(mode, flags)?;

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

    // Do not clear `pending` here: a failed `flush_pending` may re-queue postings for retry, and
    // the next update call must be able to flush them.

    let mut last: PlanQueryResult = PlanQueryResult::default();
    for stmt in block.iter_statements() {
        if matches!(stmt, Statement::Session(_)) {
            continue;
        }
        let plan =
            build_statement_plan(stmt, None).map_err(|e| GqlRunError::Plan(e.to_string()))?;
        if plan.has_dml() {
            store.execute_plan_mutations(&plan)?;
            pending::flush_pending(index).await?;
        } else {
            last = execute_plan_query(&store, &plan, parameters, index).await?;
        }
    }
    Ok(last)
}

/// Run a prepared program from [`GraphStore::prepared_query_get`].
///
/// Does not inspect caller RBAC: restrict access by routing callers to the composite-query vs update
/// canister methods (`prepared_execute_query` vs `prepared_execute_update`) only.
pub async fn run_prepared_gql(
    store: GraphStore,
    record: &PreparedQueryRecord,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
) -> Result<PlanQueryResult, GqlRunError> {
    let program = &record.program;
    let flags = classify_program(program);
    if flags.requires_write_path() != record.requires_write_path {
        return Err(GqlRunError::Plan(
            "prepared query write-path metadata does not match program".into(),
        ));
    }
    enforce_execution_mode(mode, flags)?;

    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| GqlRunError::Parse("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| GqlRunError::Parse("missing statement block".into()))?;

    // Do not clear `pending` here: a failed `flush_pending` may re-queue postings for retry, and
    // the next update call must be able to flush them.

    let mut last: PlanQueryResult = PlanQueryResult::default();
    for stmt in block.iter_statements() {
        if matches!(stmt, Statement::Session(_)) {
            continue;
        }
        let plan =
            build_statement_plan(stmt, None).map_err(|e| GqlRunError::Plan(e.to_string()))?;
        if plan.has_dml() {
            store.execute_plan_mutations(&plan)?;
            pending::flush_pending(index).await?;
        } else {
            last = execute_plan_query(&store, &plan, parameters, index).await?;
        }
    }
    Ok(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn update_mode_rejects_read_only_program() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:Person) RETURN n",
            &params,
            Role::Read,
            None,
            GqlCanisterExecutionMode::Update,
        ))
        .expect_err("expected plan error");
        assert!(
            err.to_string().contains("gql_query"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn composite_query_rejects_mutation_program() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (n:Person {age: 1})",
            &params,
            Role::Write,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
        ))
        .expect_err("expected plan error");
        assert!(
            err.to_string().contains("gql_execute"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn adhoc_read_rejects_insert_on_update_entrypoint() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (n:Person {age: 1})",
            &params,
            Role::Read,
            None,
            GqlCanisterExecutionMode::Update,
        ))
        .expect_err("expected auth error");
        assert!(err.to_string().contains("Write"), "unexpected error: {err}");
    }

    #[test]
    fn adhoc_write_allows_insert_via_update_entrypoint() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (n:TxTest {age: 1})",
            &params,
            Role::Write,
            None,
            GqlCanisterExecutionMode::Update,
        ))
        .expect("insert");
        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:TxTest) RETURN n.age",
            &params,
            Role::Read,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
        ))
        .expect("match");
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0].get("n.age"), Some(&Value::Int64(1)));
    }

    #[test]
    fn prepared_composite_rejects_mutation_program() {
        let store = GraphStore::new();
        store
            .prepared_query_register("prep_ins".into(), "INSERT (n:PrepMut {age: 1})")
            .expect("register");
        let record = store.prepared_query_get("prep_ins").expect("get");
        let params = BTreeMap::new();
        let err = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
        ))
        .expect_err("expected plan error");
        assert!(
            err.to_string().contains("gql_execute"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn prepared_update_rejects_read_only_program() {
        let store = GraphStore::new();
        store
            .prepared_query_register("prep_ro".into(), "MATCH (n:PrepRo) RETURN n")
            .expect("register");
        let record = store.prepared_query_get("prep_ro").expect("get");
        let params = BTreeMap::new();
        let err = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::Update,
        ))
        .expect_err("expected plan error");
        assert!(
            err.to_string().contains("gql_query"),
            "unexpected error: {err}"
        );
    }
}
