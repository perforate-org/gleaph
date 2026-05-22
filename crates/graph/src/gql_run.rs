//! Parse, plan, and execute GQL against [`GraphStore`] (library / unit tests; RBAC on router).

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::pending;
use crate::plan::{
    PlanBinding, PlanMutationExecutor, PlanQueryResult, PlanQueryRow, execute_plan_query,
    execute_plan_query_bindings, execute_plan_query_bindings_with_initial_rows,
};
use gleaph_gql::Value;
use gleaph_gql::ast::{Statement, StatementBlock};
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::wire::decode_plan_bundle;
use gleaph_gql_planner::{PlanBuildOptions, build_statement_plan_with_options};
use gleaph_graph_kernel::plan_exec::GqlExecutionMode as KernelGqlExecutionMode;
use gleaph_graph_kernel::plan_exec::SeedBindingsWire;
use gleaph_graph_prepared::PreparedQueryRecord;
use ic_stable_lara::VertexId;

use crate::plan::query::GLEAPH_PATH_EXTENSION_HANDLER;
use std::collections::BTreeMap;

pub fn kernel_execution_mode(mode: KernelGqlExecutionMode) -> GqlCanisterExecutionMode {
    match mode {
        KernelGqlExecutionMode::Query => GqlCanisterExecutionMode::CompositeQuery,
        KernelGqlExecutionMode::Update => GqlCanisterExecutionMode::Update,
    }
}

fn gleaph_plan_options() -> PlanBuildOptions<'static> {
    PlanBuildOptions {
        stats: None,
        path_extensions: &GLEAPH_PATH_EXTENSION_HANDLER,
    }
}

fn plan_statement(
    stmt: &Statement,
) -> Result<gleaph_gql_planner::PhysicalPlan, gleaph_gql_planner::PlannerError> {
    build_statement_plan_with_options(stmt, gleaph_plan_options(), &NoSchema)
}

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
    Query(crate::plan::PlanQueryError),
    Mutation(crate::plan::PlanMutationError),
}

impl std::fmt::Display for GqlRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(s) => write!(f, "parse error: {s}"),
            Self::Plan(s) => write!(f, "plan error: {s}"),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionReadMaterialize {
    /// Existing behavior: last read statement returns fully materialized [`Value`] rows.
    Full,
    /// Skip per-row [`Value`] construction (paths, vertex records, …) when only a row count is needed.
    LastReadRowCountOnly,
    /// Keep last read statement as [`PlanQueryRow`] bindings (paths stay lazy until the caller materializes).
    LastReadBindingsOnly,
}

struct TransactionBlockRun {
    last_query_rows: PlanQueryResult,
    last_read_row_count: usize,
    last_read_plan_rows: Vec<PlanQueryRow>,
}

/// Walk `block` in program order: run DML + flush pending; for read plans materialize [`Value`]
/// rows, only count rows, or retain binding rows for the last read statement.
async fn run_transaction_block(
    store: &GraphStore,
    block: &StatementBlock,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    materialize: TransactionReadMaterialize,
) -> Result<TransactionBlockRun, GqlRunError> {
    let mut last_query_rows = PlanQueryResult::default();
    let mut last_read_row_count: usize = 0;
    let mut last_read_plan_rows: Vec<PlanQueryRow> = Vec::new();
    for stmt in block.iter_statements() {
        if matches!(stmt, Statement::Session(_)) {
            continue;
        }
        let plan = plan_statement(stmt).map_err(|e| GqlRunError::Plan(e.to_string()))?;
        if plan.has_dml() {
            store.execute_plan_mutations(&plan, execution)?;
            pending::flush_pending(index).await?;
        } else {
            match materialize {
                TransactionReadMaterialize::Full => {
                    last_query_rows =
                        execute_plan_query(store, &plan, parameters, index, execution).await?;
                }
                TransactionReadMaterialize::LastReadRowCountOnly => {
                    let rows =
                        execute_plan_query_bindings(store, &plan, parameters, index, execution)
                            .await?;
                    last_read_row_count = rows.len();
                }
                TransactionReadMaterialize::LastReadBindingsOnly => {
                    last_read_plan_rows =
                        execute_plan_query_bindings(store, &plan, parameters, index, execution)
                            .await?;
                    last_read_row_count = last_read_plan_rows.len();
                }
            }
        }
    }
    Ok(TransactionBlockRun {
        last_query_rows,
        last_read_row_count,
        last_read_plan_rows,
    })
}

async fn run_adhoc_gql_transaction(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
    materialize: TransactionReadMaterialize,
) -> Result<TransactionBlockRun, GqlRunError> {
    let program = parser::parse(gql).map_err(|e| GqlRunError::Parse(e.to_string()))?;

    let flags = classify_program(&program);
    enforce_execution_mode(mode, flags)?;

    let tx = program
        .transaction_activity
        .ok_or_else(|| GqlRunError::Parse("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| GqlRunError::Parse("missing statement block".into()))?;

    // Do not clear `pending` here: a failed `flush_pending` may re-queue postings for retry, and
    // the next update call must be able to flush them.

    run_transaction_block(&store, block, parameters, index, execution, materialize).await
}

/// Ad-hoc GQL text (not prepared). Caller supplies [`GqlCanisterExecutionMode`] matching the canister entrypoint.
pub async fn run_adhoc_gql(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<PlanQueryResult, GqlRunError> {
    Ok(run_adhoc_gql_transaction(
        store,
        gql,
        parameters,
        index,
        mode,
        execution,
        TransactionReadMaterialize::Full,
    )
    .await?
    .last_query_rows)
}

/// Same as [`run_adhoc_gql`] for auth / parse / execution, but returns only the **row count** of the
/// last read statement and **does not** run [`crate::plan::query::materialize_plan_rows`] (no
/// `Value::Path` / full vertex hydration). Intended for callers that discard row payloads (e.g.
/// current IC canister `gql_query` / `gql_execute` stubs that only return `len`).
pub(crate) async fn run_adhoc_gql_last_read_row_count(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<usize, GqlRunError> {
    Ok(run_adhoc_gql_transaction(
        store,
        gql,
        parameters,
        index,
        mode,
        execution,
        TransactionReadMaterialize::LastReadRowCountOnly,
    )
    .await?
    .last_read_row_count)
}

/// Last read statement as binding rows (paths remain [`crate::plan::PlanBinding::Path`] until
/// [`crate::plan::materialize_plan_rows`] or [`PlanQueryResult::try_from_plan_rows`]).
pub async fn run_adhoc_gql_last_read_plan_rows(
    store: GraphStore,
    gql: &str,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<Vec<PlanQueryRow>, GqlRunError> {
    Ok(run_adhoc_gql_transaction(
        store,
        gql,
        parameters,
        index,
        mode,
        execution,
        TransactionReadMaterialize::LastReadBindingsOnly,
    )
    .await?
    .last_read_plan_rows)
}

/// Prepared statement block runner for [`run_prepared_gql`] / [`run_prepared_gql_last_read_row_count`].
async fn run_prepared_gql_transaction(
    store: GraphStore,
    record: &PreparedQueryRecord,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
    materialize: TransactionReadMaterialize,
) -> Result<TransactionBlockRun, GqlRunError> {
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

    run_transaction_block(&store, block, parameters, index, execution, materialize).await
}

/// Run a prepared program (in-memory record; registration lives on the router canister).
pub async fn run_prepared_gql(
    store: GraphStore,
    record: &PreparedQueryRecord,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<PlanQueryResult, GqlRunError> {
    Ok(run_prepared_gql_transaction(
        store,
        record,
        parameters,
        index,
        mode,
        execution,
        TransactionReadMaterialize::Full,
    )
    .await?
    .last_query_rows)
}

fn seed_initial_rows(
    store: &GraphStore,
    seeds: &SeedBindingsWire,
) -> Result<(Vec<PlanQueryRow>, bool), GqlRunError> {
    let mut all_rows = Vec::new();
    for entry in &seeds.entries {
        for &vid in &entry.local_vertex_ids {
            let vertex_id = VertexId::from(vid);
            let Some(vertex) = store.vertex(vertex_id) else {
                continue;
            };
            if vertex.is_tombstone() {
                continue;
            }
            let mut row = PlanQueryRow::new();
            row.insert(entry.variable.clone(), PlanBinding::Vertex(vertex_id));
            all_rows.push(row);
        }
    }
    Ok((all_rows, true))
}

async fn run_wire_plans(
    store: &GraphStore,
    plans: &[gleaph_gql_planner::PhysicalPlan],
    requires_write_path: bool,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
    seeds: Option<SeedBindingsWire>,
    materialize: TransactionReadMaterialize,
) -> Result<TransactionBlockRun, GqlRunError> {
    crate::plan_wire_guard::validate_wire_plan_execution(
        match mode {
            GqlCanisterExecutionMode::CompositeQuery => {
                gleaph_graph_kernel::plan_exec::GqlExecutionMode::Query
            }
            GqlCanisterExecutionMode::Update => {
                gleaph_graph_kernel::plan_exec::GqlExecutionMode::Update
            }
        },
        plans,
        requires_write_path,
    )
    .map_err(|e| GqlRunError::Plan(e.0))?;

    let mut last_query_rows = PlanQueryResult::default();
    let mut last_read_row_count: usize = 0;
    let mut last_read_plan_rows: Vec<PlanQueryRow> = Vec::new();

    let (mut seed_rows, mut skip_index) = if let Some(ref s) = seeds {
        seed_initial_rows(&store, s)?
    } else {
        (Vec::new(), false)
    };

    for (i, plan) in plans.iter().enumerate() {
        if plan.has_dml() {
            store.execute_plan_mutations(plan, execution)?;
            pending::flush_pending(index).await?;
            skip_index = false;
            seed_rows.clear();
        } else {
            let use_seeds = skip_index && !seed_rows.is_empty() && i == 0;
            let initial = if use_seeds {
                std::mem::take(&mut seed_rows)
            } else {
                vec![crate::plan::empty_row_for_plan(plan)]
            };
            let skip = use_seeds;
            match materialize {
                TransactionReadMaterialize::Full => {
                    last_query_rows = execute_plan_query_with_rows(
                        store, plan, parameters, index, execution, initial, skip,
                    )
                    .await?;
                }
                TransactionReadMaterialize::LastReadRowCountOnly => {
                    let rows = execute_plan_query_bindings_with_initial_rows(
                        store, plan, parameters, index, execution, initial, skip,
                    )
                    .await?;
                    last_read_row_count = rows.len();
                }
                TransactionReadMaterialize::LastReadBindingsOnly => {
                    last_read_plan_rows = execute_plan_query_bindings_with_initial_rows(
                        store, plan, parameters, index, execution, initial, skip,
                    )
                    .await?;
                    last_read_row_count = last_read_plan_rows.len();
                }
            }
        }
    }
    Ok(TransactionBlockRun {
        last_query_rows,
        last_read_row_count,
        last_read_plan_rows,
    })
}

async fn execute_plan_query_with_rows(
    store: &GraphStore,
    plan: &gleaph_gql_planner::PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    initial_rows: Vec<PlanQueryRow>,
    skip_leading_index_scan: bool,
) -> Result<PlanQueryResult, GqlRunError> {
    let rows = execute_plan_query_bindings_with_initial_rows(
        store,
        plan,
        parameters,
        index,
        execution,
        initial_rows,
        skip_leading_index_scan,
    )
    .await?;
    Ok(PlanQueryResult {
        rows: crate::plan::materialize_plan_rows(store, &rows)?,
    })
}

/// Run a wire-encoded plan bundle from the router (no parse/plan on graph).
pub async fn run_wire_plan_last_read_row_count(
    store: GraphStore,
    plan_blob: &[u8],
    parameters: &BTreeMap<String, Value>,
    mode: GqlCanisterExecutionMode,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    seeds: Option<SeedBindingsWire>,
) -> Result<usize, GqlRunError> {
    let (requires_write, plans) =
        decode_plan_bundle(plan_blob).map_err(|e| GqlRunError::Plan(e.to_string()))?;
    Ok(run_wire_plans(
        &store,
        &plans,
        requires_write,
        parameters,
        index,
        mode,
        execution,
        seeds,
        TransactionReadMaterialize::LastReadRowCountOnly,
    )
    .await?
    .last_read_row_count)
}

/// Run a wire-encoded program (router → graph); skips parser, still plans locally.
pub async fn run_program_gql_last_read_row_count(
    store: GraphStore,
    record: &PreparedQueryRecord,
    parameters: &BTreeMap<String, Value>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<usize, GqlRunError> {
    run_prepared_gql_last_read_row_count(store, record, parameters, None, mode, execution).await
}

/// Prepared counterpart to [`run_adhoc_gql_last_read_row_count`].
pub(crate) async fn run_prepared_gql_last_read_row_count(
    store: GraphStore,
    record: &PreparedQueryRecord,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<usize, GqlRunError> {
    Ok(run_prepared_gql_transaction(
        store,
        record,
        parameters,
        index,
        mode,
        execution,
        TransactionReadMaterialize::LastReadRowCountOnly,
    )
    .await?
    .last_read_row_count)
}

/// Prepared counterpart to [`run_adhoc_gql_last_read_plan_rows`].
pub async fn run_prepared_gql_last_read_plan_rows(
    store: GraphStore,
    record: &PreparedQueryRecord,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
) -> Result<Vec<PlanQueryRow>, GqlRunError> {
    Ok(run_prepared_gql_transaction(
        store,
        record,
        parameters,
        index,
        mode,
        execution,
        TransactionReadMaterialize::LastReadBindingsOnly,
    )
    .await?
    .last_read_plan_rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gql_execution_context::GqlExecutionContext;
    use gleaph_gql::Value;
    use gleaph_graph_prepared::{PreparedQueryRecord, compile_prepared_source};

    fn compile_prepared(source: &str) -> PreparedQueryRecord {
        let program = compile_prepared_source(source).expect("compile");
        let requires_write_path = classify_program(&program).requires_write_path();
        PreparedQueryRecord {
            program,
            requires_write_path,
        }
    }

    #[test]
    fn update_mode_rejects_read_only_program() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:Person) RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
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
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("expected plan error");
        assert!(
            err.to_string().contains("gql_execute"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn adhoc_write_allows_insert_via_update_entrypoint() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (n:TxTest {age: 1})",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("insert");
        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:TxTest) RETURN n.age",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("match");
        assert_eq!(q.rows.len(), 1);
        assert_eq!(q.rows[0].get("n.age"), Some(&Value::Int64(1)));
    }

    #[test]
    fn prepared_composite_rejects_mutation_program() {
        let store = GraphStore::new();
        let record = compile_prepared("INSERT (n:PrepMut {age: 1})");
        let params = BTreeMap::new();
        let err = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
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
        let record = compile_prepared("MATCH (n:PrepRo) RETURN n");
        let params = BTreeMap::new();
        let err = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect_err("expected plan error");
        assert!(
            err.to_string().contains("gql_query"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn msg_caller_return_with_execution_context() {
        use gleaph_gql_ic::{PrincipalValue, value_as_principal};

        let p = candid::Principal::from_text("2vxsx-fae").expect("principal");
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let q = pollster::block_on(run_adhoc_gql(
            store,
            "RETURN MSG_CALLER() AS c",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext { caller: Some(p) },
        ))
        .expect("query");
        assert_eq!(q.rows.len(), 1);
        let cell = q.rows[0].get("c").expect("column c");
        let Value::Extension(ext) = cell else {
            panic!("expected extension, got {cell:?}");
        };
        let pv = ext.as_any().downcast_ref::<PrincipalValue>().expect("pv");
        assert_eq!(pv.0, p);
        assert_eq!(value_as_principal(cell), Some(p));
    }

    #[test]
    fn msg_caller_insert_mutation_stores_principal() {
        use gleaph_gql_ic::{PrincipalValue, value_as_principal};

        let p = candid::Principal::from_text("2vxsx-fae").expect("principal");
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (n:MsgCallerOwner {owner: MSG_CALLER()})",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext { caller: Some(p) },
        ))
        .expect("insert");
        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:MsgCallerOwner) RETURN n.owner AS o",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext { caller: Some(p) },
        ))
        .expect("read back");
        assert_eq!(q.rows.len(), 1);
        let o = q.rows[0].get("o").expect("o");
        let Value::Extension(ext) = o else {
            panic!("expected extension");
        };
        ext.as_any().downcast_ref::<PrincipalValue>().expect("pv");
        assert_eq!(value_as_principal(o), Some(p));
    }

    #[test]
    fn msg_caller_prepared_uses_execute_caller_not_registration() {
        use gleaph_gql_ic::value_as_principal;

        let exec = candid::Principal::from_text("2vxsx-fae").expect("exec");
        let store = GraphStore::new();
        let record = compile_prepared("RETURN MSG_CALLER() AS c");
        let params = BTreeMap::new();
        let q = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext { caller: Some(exec) },
        ))
        .expect("exec prepared");
        assert_eq!(
            value_as_principal(q.rows[0].get("c").expect("c")),
            Some(exec)
        );
    }

    #[test]
    fn last_read_row_count_matches_materialized_row_count() {
        let params = BTreeMap::new();
        let gql = "RETURN 1 AS x";
        let count = pollster::block_on(run_adhoc_gql_last_read_row_count(
            GraphStore::new(),
            gql,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("row count");
        let full = pollster::block_on(run_adhoc_gql(
            GraphStore::new(),
            gql,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("materialized");
        assert_eq!(count, full.rows.len());
    }

    #[test]
    fn last_read_plan_rows_materialize_same_as_run_adhoc_gql() {
        use crate::plan::PlanQueryResult;

        let store = GraphStore::new();
        let params = BTreeMap::new();
        let gql = "RETURN 1 AS x";
        let binding_rows = pollster::block_on(run_adhoc_gql_last_read_plan_rows(
            GraphStore::new(),
            gql,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("bindings");
        let from_rows =
            PlanQueryResult::try_from_plan_rows(&store, &binding_rows).expect("materialize");
        let direct = pollster::block_on(run_adhoc_gql(
            GraphStore::new(),
            gql,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("direct");
        assert_eq!(from_rows, direct);
    }

    #[test]
    fn msg_caller_rejects_wrong_arity_at_execution() {
        let p = candid::Principal::from_text("2vxsx-fae").expect("principal");
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "RETURN MSG_CALLER(1) AS c",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext { caller: Some(p) },
        ))
        .expect_err("arity");
        let s = err.to_string();
        assert!(
            s.contains("expects 0 argument") && s.contains("got 1"),
            "{s}"
        );
    }

    #[test]
    fn msg_caller_rejects_distinct() {
        let p = candid::Principal::from_text("2vxsx-fae").expect("principal");
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "RETURN MSG_CALLER(DISTINCT) AS c",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext { caller: Some(p) },
        ))
        .expect_err("distinct");
        assert!(
            err.to_string().contains("does not support DISTINCT"),
            "{err}"
        );
    }

    #[test]
    fn msg_caller_requires_caller_without_execution_context() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "RETURN MSG_CALLER() AS c",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("no caller");
        assert!(
            err.to_string()
                .contains("requires a canister caller context"),
            "{err}"
        );
    }

    fn setup_gql_weighted_graph(store: &GraphStore) {
        use crate::facade::mutation_executor::GraphMutationExecutor;
        use gleaph_graph_kernel::entry::{EdgeWeightProfile, WeightEncoding};
        let a = store
            .insert_vertex_named(["WgtGqlA"], Vec::<(&str, Value)>::new())
            .expect("a");
        let b = store
            .insert_vertex_named(["WgtGqlB"], Vec::<(&str, Value)>::new())
            .expect("b");
        let c = store
            .insert_vertex_named(["WgtGqlC"], Vec::<(&str, Value)>::new())
            .expect("c");
        let label_id = store
            .get_or_insert_edge_label_id("WgtGqlRoad")
            .expect("label");
        store
            .set_edge_label_weight_profile(
                label_id,
                EdgeWeightProfile {
                    encoding: WeightEncoding::RawU16,
                },
            )
            .expect("profile");
        store
            .insert_directed_edge_with_inline_value(a, b, Some(label_id), 1)
            .expect("a->b");
        store
            .insert_directed_edge_with_inline_value(b, c, Some(label_id), 1)
            .expect("b->c");
        store
            .insert_directed_edge_with_inline_value(a, c, Some(label_id), 100)
            .expect("a->c");
    }

    fn path_len(value: Option<&Value>) -> usize {
        match value {
            Some(Value::Path(elements)) => elements.len(),
            other => panic!("expected path value, got {other:?}"),
        }
    }

    #[test]
    fn adhoc_gleaph_cost_selects_weighted_shortest_path() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT(e) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn prepared_gleaph_cost_selects_weighted_shortest_path() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let record = compile_prepared(
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT(e) RETURN p",
        );
        let params = BTreeMap::new();
        let out = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("prepared weighted gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn adhoc_gleaph_cost_rejects_shortest_k() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH SHORTEST 2 (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT(e) RETURN a",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("weighted shortest k");
        assert!(err.to_string().contains("weighted SHORTEST k"), "{err}");
    }

    #[test]
    fn adhoc_gleaph_cost_rejects_bare_edge_variable() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY e RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("bare edge variable cost");
        match &err {
            GqlRunError::Plan(msg) => {
                assert!(
                    msg.contains("bare edge variable"),
                    "expected plan rejection, got: {err}"
                );
            }
            other => panic!("expected plan error, got: {other}"),
        }
    }

    #[test]
    fn adhoc_gleaph_cost_rejects_binary_edge_var_misuse() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY e * 2 RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("binary edge variable cost");
        match &err {
            GqlRunError::Plan(msg) => {
                assert!(
                    msg.contains("inside GLEAPH.WEIGHT"),
                    "expected plan rejection, got: {err}"
                );
            }
            other => panic!("expected plan error, got: {other}"),
        }
    }

    #[test]
    fn adhoc_gleaph_cost_rejects_case_operand_edge_var_misuse() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) \
             GLEAPH.COST BY CASE e WHEN NULL THEN GLEAPH.WEIGHT(e) ELSE GLEAPH.WEIGHT(e) END RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("case operand edge variable cost");
        match &err {
            GqlRunError::Plan(msg) => {
                assert!(
                    msg.contains("inside GLEAPH.WEIGHT"),
                    "expected plan rejection, got: {err}"
                );
            }
            other => panic!("expected plan error, got: {other}"),
        }
    }

    #[test]
    fn adhoc_gleaph_cost_rejects_case_when_condition_edge_var_misuse() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let err = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) \
             GLEAPH.COST BY CASE WHEN e THEN GLEAPH.WEIGHT(e) ELSE GLEAPH.WEIGHT(e) END RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect_err("case when condition edge variable cost");
        match &err {
            GqlRunError::Plan(msg) => {
                assert!(
                    msg.contains("inside GLEAPH.WEIGHT"),
                    "expected plan rejection, got: {err}"
                );
            }
            other => panic!("expected plan error, got: {other}"),
        }
    }

    #[test]
    fn adhoc_gleaph_cost_abs_wrapped_weight_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY ABS(GLEAPH.WEIGHT(e)) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("abs-wrapped weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn prepared_gleaph_cost_parameterized_scale_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let record = compile_prepared(
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT(e) * $scale RETURN p",
        );
        let mut params = BTreeMap::new();
        params.insert("$scale".into(), Value::Float64(1.0));
        let out = pollster::block_on(run_prepared_gql(
            store,
            &record,
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("parameterized weighted gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn adhoc_gleaph_cost_floor_wrapped_weight_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY FLOOR(GLEAPH.WEIGHT(e)) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("floor-wrapped weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn adhoc_gleaph_cost_coalesce_wrapped_weight_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY COALESCE(GLEAPH.WEIGHT(e), 1.0) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("coalesce-wrapped weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn adhoc_gleaph_cost_cast_wrapped_weight_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY CAST(GLEAPH.WEIGHT(e) AS FLOAT32) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("cast-wrapped weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn adhoc_gleaph_cost_parenthesized_weight_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT((e)) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("parenthesized weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    #[test]
    fn adhoc_gleaph_cost_triple_parenthesized_weight_plans_and_runs() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH p = ANY SHORTEST (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT(((e))) RETURN p",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("triple-parenthesized weighted adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(path_len(out.rows[0].get("p")), 5);
    }

    fn setup_gql_reused_dst_graph(store: &GraphStore) {
        use crate::facade::mutation_executor::GraphMutationExecutor;
        let a = store
            .insert_vertex_named(["ReuseGqlA"], [("name", Value::Text("anchor".into()))])
            .expect("anchor");
        let b = store
            .insert_vertex_named(["ReuseGqlB"], [("name", Value::Text("other".into()))])
            .expect("neighbor");
        store
            .insert_directed_edge_named(a, a, Some("ReuseGqlRel"), Vec::<(&str, Value)>::new())
            .expect("self-loop");
        store
            .insert_directed_edge_named(a, b, Some("ReuseGqlRel"), Vec::<(&str, Value)>::new())
            .expect("out-edge");
    }

    #[test]
    fn adhoc_reused_dst_expand_only_keeps_self_loop() {
        let store = GraphStore::new();
        setup_gql_reused_dst_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (a:ReuseGqlA)-[:ReuseGqlRel]->(a) RETURN a.name AS name",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("reused dst adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].get("name"), Some(&Value::Text("anchor".into())));
    }

    fn setup_gql_reused_dst_relabeled_graph(store: &GraphStore) {
        use crate::facade::mutation_executor::GraphMutationExecutor;
        let a = store
            .insert_vertex_named(
                ["ReuseGqlPerson", "ReuseGqlUser"],
                [("name", Value::Text("anchor".into()))],
            )
            .expect("anchor");
        let b = store
            .insert_vertex_named(["ReuseGqlPerson"], [("name", Value::Text("other".into()))])
            .expect("neighbor");
        store
            .insert_directed_edge_named(a, a, Some("ReuseGqlRel"), Vec::<(&str, Value)>::new())
            .expect("self-loop");
        store
            .insert_directed_edge_named(a, b, Some("ReuseGqlRel"), Vec::<(&str, Value)>::new())
            .expect("out-edge");
    }

    #[test]
    fn adhoc_reused_dst_relabeled_endpoints_keep_self_loop() {
        let store = GraphStore::new();
        setup_gql_reused_dst_relabeled_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (a:ReuseGqlPerson)-[:ReuseGqlRel]->(a:ReuseGqlUser) RETURN a.name AS name",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("reused relabeled dst adhoc gql");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0].get("name"), Some(&Value::Text("anchor".into())));
    }
}
