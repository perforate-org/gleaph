//! Parse, plan, and execute GQL against [`GraphStore`] (library / unit tests; RBAC on router).

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::{label_pending, pending};
use crate::plan::{
    PlanBinding, PlanMutationBindings, PlanQueryResult, PlanQueryRow, SeededMutationRow,
    execute_mutation_tail_async, execute_plan_query, execute_plan_query_bindings,
    execute_plan_query_bindings_with_initial_rows, plan_contains_gleaph_finalize_call,
    read_prefix_len,
};
use gleaph_gql::Value;
use gleaph_gql::ast::{Statement, StatementBlock};
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::wire::decode_plan_bundle;
use gleaph_gql_planner::{PlanBuildOptions, build_statement_plan_with_options};
use gleaph_graph_kernel::federation::{ClaimId, EffectId, UniqueEffectOp, UniqueEffectReceipt};
use gleaph_graph_kernel::plan_exec::{
    GqlExecutionMode as KernelGqlExecutionMode, LabelStatsDelta, MutationId, SeedBindingsWire,
    ShardEventSeq, UniqueClaimDispatch,
};
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

fn plan_needs_mutation_executor(plan: &gleaph_gql_planner::PhysicalPlan) -> bool {
    plan.has_dml() || plan_contains_gleaph_finalize_call(&plan.ops)
}

/// Project a read-phase binding row to the vertex/edge handles its mutation tail can seed.
fn seeded_mutation_row(row: &PlanQueryRow) -> SeededMutationRow {
    let mut seed = SeededMutationRow::default();
    for (name, binding) in row.iter() {
        match binding {
            PlanBinding::Vertex(vertex_id) => {
                seed.vertices.insert(name.to_string(), *vertex_id);
            }
            PlanBinding::Edge(edge) => {
                seed.edges.insert(name.to_string(), edge.handle);
            }
            _ => {}
        }
    }
    seed
}

/// The read-prefix sub-plan (leading ops before the first mutation op). The full plan's
/// binding layout and annotations are retained — the mutation-tail vars become unused slots.
fn read_prefix_plan(
    plan: &gleaph_gql_planner::PhysicalPlan,
    prefix_len: usize,
) -> gleaph_gql_planner::PhysicalPlan {
    let mut read_plan = plan.clone();
    read_plan.ops.truncate(prefix_len);
    read_plan
}

/// Execute a DML plan in two phases (ADR 0029 §1): run the read prefix (with index access)
/// to bind matched variables, then apply the mutation tail once per binding row in the
/// shard-local canonical segment. `router_seed` carries router-supplied seed rows plus the
/// leading-anchor skip flag; `None` means run the read prefix from a single empty row.
async fn execute_dml_plan_async(
    store: &GraphStore,
    plan: &gleaph_gql_planner::PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    router_seed: Option<(Vec<PlanQueryRow>, bool)>,
) -> Result<PlanMutationBindings, GqlRunError> {
    let seed_rows = read_phase_seed_rows(store, plan, parameters, index, &execution, router_seed)
        .await?
        .unwrap_or_default();
    let mutation_ops = &plan.ops[read_prefix_len(&plan.ops)..];
    // The mutation tail intentionally runs with empty parameters, matching the prior
    // single-pass mutation path; $-parameters are resolved in the read phase.
    let empty_params = BTreeMap::new();
    Ok(
        execute_mutation_tail_async(store, mutation_ops, &seed_rows, &empty_params, execution)
            .await?,
    )
}

/// Run a DML plan's read prefix and project the result to mutation seed rows. Returns `None`
/// when the plan has no read prefix (bare `INSERT`): the caller runs the tail once unseeded.
async fn read_phase_seed_rows(
    store: &GraphStore,
    plan: &gleaph_gql_planner::PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    execution: &GqlExecutionContext,
    router_seed: Option<(Vec<PlanQueryRow>, bool)>,
) -> Result<Option<Vec<SeededMutationRow>>, GqlRunError> {
    let prefix_len = read_prefix_len(&plan.ops);
    if prefix_len == 0 {
        return Ok(None);
    }
    let read_plan = read_prefix_plan(plan, prefix_len);
    let (initial_rows, skip_leading) = match router_seed {
        Some((rows, skip)) => (rows, skip),
        None => (vec![crate::plan::empty_row_for_plan(&read_plan)], false),
    };
    let rows = execute_plan_query_bindings_with_initial_rows(
        store,
        &read_plan,
        parameters,
        index,
        execution.clone(),
        initial_rows,
        skip_leading,
    )
    .await?;
    Ok(Some(rows.iter().map(seeded_mutation_row).collect()))
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
    label_stats_delta: LabelStatsDelta,
    emitted_delta_first_seq: Option<ShardEventSeq>,
    emitted_delta_last_seq: Option<ShardEventSeq>,
    hot_forward_vertices: Vec<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WirePlanRunResult {
    pub row_count: usize,
    pub rows_blob: Option<Vec<u8>>,
    pub hot_forward_vertices: Vec<u32>,
}

fn merge_hot_forward_vertices(target: &mut Vec<u32>, source: &[VertexId]) {
    target.extend(source.iter().map(|vid| u32::from(*vid)));
    target.sort_unstable();
    target.dedup();
}

fn procedure_value_rows_to_plan_rows(rows: &[BTreeMap<String, Value>]) -> Vec<PlanQueryRow> {
    rows.iter()
        .map(|row| {
            let mut out = PlanQueryRow::new();
            for (name, value) in row {
                out.insert(name.clone(), PlanBinding::Value(value.clone()));
            }
            out
        })
        .collect()
}

fn record_mutation_procedure_rows(
    materialize: TransactionReadMaterialize,
    rows: &[BTreeMap<String, Value>],
    last_query_rows: &mut PlanQueryResult,
    last_read_row_count: &mut usize,
    last_read_plan_rows: &mut Vec<PlanQueryRow>,
) {
    if rows.is_empty() {
        return;
    }

    *last_read_row_count = rows.len();
    match materialize {
        TransactionReadMaterialize::Full => {
            last_query_rows.rows = rows.to_vec();
            last_read_plan_rows.clear();
        }
        TransactionReadMaterialize::LastReadRowCountOnly => {
            last_read_plan_rows.clear();
        }
        TransactionReadMaterialize::LastReadBindingsOnly => {
            *last_read_plan_rows = procedure_value_rows_to_plan_rows(rows);
        }
    }
}

fn trap_wire_mutation_failure(error: crate::plan::PlanMutationError) -> ! {
    let message = format!("wire mutation failed inside DML atomic section: {error}");
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::trap(&message);
    }
    #[cfg(not(target_family = "wasm"))]
    {
        panic!("{message}");
    }
}

/// Pins one `Acquire` receipt per dispatched uniqueness claim, all owned by the single element the
/// segment created (ADR 0030 slice 5). The Router's admission gate guarantees this segment is a
/// statically single-element INSERT, so every claim shares one `owner_element_id`; any other shape
/// is an admission/dispatch invariant violation and traps (rolling back the whole atomic segment
/// rather than recording mismatched commit evidence).
fn emit_unique_acquires(
    store: &GraphStore,
    claims: &[UniqueClaimDispatch],
    mutation_id: Option<MutationId>,
    mutation: &PlanMutationBindings,
) {
    let Some(mutation_id) = mutation_id else {
        unique_acquire_trap("uniqueness claims dispatched without a mutation_id");
    };
    let owner_vertex = match mutation.created_vertices.as_slice() {
        [vertex_id] => *vertex_id,
        created => unique_acquire_trap(&format!(
            "expected exactly one created vertex for {} uniqueness claim(s), got {}",
            claims.len(),
            created.len()
        )),
    };
    let owner_element_id = store
        .path_vertex_element_id(owner_vertex)
        .map(|id| id.to_bytes().to_vec())
        .unwrap_or_else(|| {
            unique_acquire_trap("owner element id unavailable after insert (no global vertex id)")
        });
    for claim in claims {
        store.emit_unique_effect(UniqueEffectReceipt {
            effect_id: EffectId::new(mutation_id, claim.claim_ordinal),
            claim_id: Some(ClaimId::new(mutation_id, claim.claim_ordinal)),
            owner_element_id: owner_element_id.clone(),
            constraint_id: claim.constraint_id,
            encoded_value: claim.encoded_value.clone(),
            op: UniqueEffectOp::Acquire,
        });
    }
}

fn unique_acquire_trap(message: &str) -> ! {
    let message = format!("unique-effect Acquire emit failed inside DML atomic section: {message}");
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::trap(&message);
    }
    #[cfg(not(target_family = "wasm"))]
    {
        panic!("{message}");
    }
}

fn extend_delta_seq_range(
    first: &mut Option<ShardEventSeq>,
    last: &mut Option<ShardEventSeq>,
    seq: ShardEventSeq,
) {
    if first.is_none() {
        *first = Some(seq);
    }
    *last = Some(seq);
}

fn merge_label_stats_delta(target: &mut LabelStatsDelta, source: LabelStatsDelta) {
    for (label, delta) in source.vertex {
        merge_delta(&mut target.vertex, label, delta);
    }
    for (label, delta) in source.edge {
        merge_delta(&mut target.edge, label, delta);
    }
}

fn merge_delta<T>(target: &mut Vec<(T, i64)>, label: T, delta: i64)
where
    T: Copy + Eq,
{
    if delta == 0 {
        return;
    }
    if let Some((_, existing)) = target.iter_mut().find(|(id, _)| *id == label) {
        *existing += delta;
        if *existing == 0 {
            target.retain(|(_, value)| *value != 0);
        }
        return;
    }
    target.push((label, delta));
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
    let mut label_stats_delta = LabelStatsDelta::default();
    let mut hot_forward_vertices = Vec::new();
    for stmt in block.iter_statements() {
        if matches!(stmt, Statement::Session(_)) {
            continue;
        }
        let plan = plan_statement(stmt).map_err(|e| GqlRunError::Plan(e.to_string()))?;
        if plan_needs_mutation_executor(&plan) {
            let mutation =
                execute_dml_plan_async(store, &plan, parameters, index, execution.clone(), None)
                    .await?;
            merge_hot_forward_vertices(&mut hot_forward_vertices, &mutation.hot_forward_vertices);
            merge_label_stats_delta(&mut label_stats_delta, mutation.label_stats_delta);
            record_mutation_procedure_rows(
                materialize,
                &mutation.procedure_rows,
                &mut last_query_rows,
                &mut last_read_row_count,
                &mut last_read_plan_rows,
            );
            pending::flush_pending(index, None).await?;
            crate::index::edge_pending::flush_pending(index, None).await?;
            label_pending::flush_pending(index, None).await?;
        } else {
            match materialize {
                TransactionReadMaterialize::Full => {
                    last_query_rows =
                        execute_plan_query(store, &plan, parameters, index, execution.clone())
                            .await?;
                }
                TransactionReadMaterialize::LastReadRowCountOnly => {
                    let rows = execute_plan_query_bindings(
                        store,
                        &plan,
                        parameters,
                        index,
                        execution.clone(),
                    )
                    .await?;
                    last_read_row_count = rows.len();
                }
                TransactionReadMaterialize::LastReadBindingsOnly => {
                    last_read_plan_rows = execute_plan_query_bindings(
                        store,
                        &plan,
                        parameters,
                        index,
                        execution.clone(),
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
        label_stats_delta,
        emitted_delta_first_seq: None,
        emitted_delta_last_seq: None,
        hot_forward_vertices,
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
    use ic_stable_lara::BucketLabelKey as LaraLabelId;

    use crate::facade::EdgeHandle;
    use crate::plan::EdgeBinding;

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
        for posting in &entry.local_edge_postings {
            let handle = EdgeHandle {
                owner_vertex_id: VertexId::from(posting.owner_vertex_id),
                label_id: LaraLabelId::from_raw(posting.label_id),
                slot_index: posting.slot_index,
            };
            let Some(edge) = store
                .find_outgoing_edge_record(handle)
                .map_err(|e| GqlRunError::Plan(e.to_string()))?
            else {
                continue;
            };
            let mut row = PlanQueryRow::new();
            row.insert(
                entry.variable.clone(),
                PlanBinding::Edge(EdgeBinding::from_edge(handle, edge)),
            );
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
    mutation_id: Option<MutationId>,
) -> Result<TransactionBlockRun, GqlRunError> {
    crate::edge_payload_schema::set_execution_resolved_labels(execution.resolved_labels.clone());
    crate::element_id_encoding::set_execution_element_id_key(execution.element_id_encoding_key());
    let run_result = run_wire_plans_inner(
        store,
        plans,
        requires_write_path,
        parameters,
        index,
        mode,
        execution,
        seeds,
        materialize,
        mutation_id,
    )
    .await;
    crate::edge_payload_schema::clear_execution_resolved_labels();
    crate::element_id_encoding::clear_execution_element_id_key();
    run_result
}

/// ADR 0029 §1 canonical critical section for a single DML plan.
///
/// This segment performs **only shard-local canonical writes**: it executes the plan's mutations
/// against [`GraphStore`], appends the label-stats projection *intent* to the durable delta log,
/// and records the shard-local mutation journal as `Incomplete`. There is no inter-canister
/// `await` between these steps, so by IC execution semantics the whole segment commits — or, on
/// trap, rolls back — atomically within one message handler.
///
/// The segment deliberately takes **no `PropertyIndexLookup` handle**: all remote inputs (resolved
/// labels, properties, catalog, seed bindings) are pre-resolved by the Router into `execution`
/// before execution begins, so the critical section cannot — and must not — issue any
/// inter-canister call. Index posting *delivery* happens *after* this segment via `flush_pending`
/// (the asynchronous projection boundary), never inside it. The missing index parameter is the
/// structural enforcement of the "no remote call inside the critical section" invariant.
///
/// Matched-variable mutations (`MATCH ... DELETE`/`SET`/etc.) run their **read prefix in a
/// separate read phase** (with index access) *before* this segment; the resulting bindings
/// arrive as `seed_rows`, so this segment only applies the write-only mutation tail.
async fn apply_canonical_mutation_segment(
    store: &GraphStore,
    mutation_ops: &[gleaph_gql_planner::plan::PlanOp],
    seed_rows: &[SeededMutationRow],
    execution: GqlExecutionContext,
    mutation_id: Option<MutationId>,
    emitted_delta_first_seq: &mut Option<ShardEventSeq>,
    emitted_delta_last_seq: &mut Option<ShardEventSeq>,
) -> Result<PlanMutationBindings, GqlRunError> {
    let empty_params = BTreeMap::new();
    let unique_claims = execution.unique_claims.clone();
    let mutation =
        match execute_mutation_tail_async(store, mutation_ops, seed_rows, &empty_params, execution)
            .await
        {
            Ok(mutation) => mutation,
            Err(error) => trap_wire_mutation_failure(error),
        };
    // ADR 0030 slice 5: pin the cross-shard uniqueness `Acquire` receipts for the element created
    // in this segment. This runs inside the same no-`await` canonical section as the write above, so
    // the receipts commit (or roll back on trap) atomically with the canonical mutation.
    if !unique_claims.is_empty() {
        emit_unique_acquires(store, &unique_claims, mutation_id, &mutation);
    }
    let has_delta = !mutation.label_stats_delta.vertex.is_empty()
        || !mutation.label_stats_delta.edge.is_empty();
    if let Some(mutation_id) = mutation_id
        && has_delta
    {
        let event = store
            .commit_append_label_stats_delta(mutation_id, mutation.label_stats_delta.clone())
            .map_err(GqlRunError::Plan)?;
        extend_delta_seq_range(
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            event.shard_event_seq,
        );
    }
    if let Some(mutation_id) = mutation_id {
        store.commit_record_incomplete_mutation_journal(
            mutation_id,
            *emitted_delta_first_seq,
            *emitted_delta_last_seq,
        );
    }
    Ok(mutation)
}

async fn run_wire_plans_inner(
    store: &GraphStore,
    plans: &[gleaph_gql_planner::PhysicalPlan],
    requires_write_path: bool,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    mode: GqlCanisterExecutionMode,
    execution: GqlExecutionContext,
    seeds: Option<SeedBindingsWire>,
    materialize: TransactionReadMaterialize,
    mutation_id: Option<MutationId>,
) -> Result<TransactionBlockRun, GqlRunError> {
    if let Some(mutation_id) = mutation_id
        && let Some(journal) = store.mutation_journal_entry(mutation_id)
    {
        if journal.is_completed() {
            return Ok(TransactionBlockRun {
                last_query_rows: PlanQueryResult::default(),
                last_read_row_count: journal.row_count as usize,
                last_read_plan_rows: Vec::new(),
                label_stats_delta: LabelStatsDelta::default(),
                emitted_delta_first_seq: journal.emitted_delta_first_seq,
                emitted_delta_last_seq: journal.emitted_delta_last_seq,
                hot_forward_vertices: journal.hot_forward_vertices.clone(),
            });
        }
        return Err(GqlRunError::Plan(format!(
            "mutation {mutation_id} was already applied locally but did not complete; advance label stats projection instead"
        )));
    }

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

    if requires_write_path && mutation_id.is_none() {
        return Err(GqlRunError::Plan(
            "wire DML execution requires mutation_id".into(),
        ));
    }

    let mut last_query_rows = PlanQueryResult::default();
    let mut last_read_row_count: usize = 0;
    let mut last_read_plan_rows: Vec<PlanQueryRow> = Vec::new();
    let mut label_stats_delta = LabelStatsDelta::default();
    let mut emitted_delta_first_seq = None;
    let mut emitted_delta_last_seq = None;
    let mut hot_forward_vertices = Vec::new();

    let (mut seed_rows, mut skip_index) = if let Some(ref s) = seeds {
        seed_initial_rows(store, s)?
    } else {
        (Vec::new(), false)
    };

    for (plan_idx, plan) in plans.iter().enumerate() {
        if plan_needs_mutation_executor(plan) {
            // ADR 0029: read phase (with index) binds matched variables; consumes the router's
            // leading-anchor seed rows once, just like a read-only plan would.
            let router_seed = (skip_index && !seed_rows.is_empty())
                .then(|| (std::mem::take(&mut seed_rows), true));
            let mutation_seed_rows =
                read_phase_seed_rows(store, plan, parameters, index, &execution, router_seed)
                    .await?
                    .unwrap_or_default();
            // ADR 0029 §1: shard-local canonical critical section (no inter-canister call).
            let mutation = apply_canonical_mutation_segment(
                store,
                &plan.ops[read_prefix_len(&plan.ops)..],
                &mutation_seed_rows,
                execution.clone(),
                mutation_id,
                &mut emitted_delta_first_seq,
                &mut emitted_delta_last_seq,
            )
            .await?;
            merge_label_stats_delta(&mut label_stats_delta, mutation.label_stats_delta);
            merge_hot_forward_vertices(&mut hot_forward_vertices, &mutation.hot_forward_vertices);
            record_mutation_procedure_rows(
                materialize,
                &mutation.procedure_rows,
                &mut last_query_rows,
                &mut last_read_row_count,
                &mut last_read_plan_rows,
            );
            // Asynchronous projection delivery (ADR 0029) — OUTSIDE the canonical critical
            // section above. The canonical store mutation and the label-stats delta are already
            // durable; these inter-canister flushes only deliver derived index postings.
            // Flush all three pending buffers (forward / edge / label). They are evaluated
            // even when an earlier one defers, so every posting is either applied or durably
            // journaled for repair before we consider completing the mutation (ADR 0024).
            let mut index_deferred = false;
            for flush_result in [
                pending::flush_pending(index, mutation_id).await,
                crate::index::edge_pending::flush_pending(index, mutation_id).await,
                label_pending::flush_pending(index, mutation_id).await,
            ] {
                match flush_result {
                    Ok(()) => {}
                    Err(crate::plan::PlanQueryError::IndexFlushDeferred { .. }) => {
                        index_deferred = true;
                    }
                    Err(other) => return Err(other.into()),
                }
            }
            if index_deferred {
                // ADR 0024: a repair-journaled flush is not a mutation failure. The store
                // mutation and label-stats deltas are durable and the index converges via
                // the maintenance timer (ADR 0023). Finalize the mutation journal now so it
                // is not wedged Incomplete forever — but only when no unexecuted DML remains
                // (completing early would silently drop later writes on idempotent replay).
                if plans[plan_idx + 1..]
                    .iter()
                    .any(plan_needs_mutation_executor)
                {
                    return Err(GqlRunError::Plan(
                        "index flush deferred for repair with unexecuted DML remaining; \
                         mutation left incomplete for retry"
                            .into(),
                    ));
                }
                // Trailing read-only plans are intentionally skipped: their index-served
                // reads could observe the pre-repair index state.
                if let Some(mutation_id) = mutation_id {
                    store.commit_record_completed_mutation_journal(
                        mutation_id,
                        last_read_row_count as u64,
                        emitted_delta_first_seq,
                        emitted_delta_last_seq,
                        hot_forward_vertices.clone(),
                    );
                }
                return Ok(TransactionBlockRun {
                    last_query_rows,
                    last_read_row_count,
                    last_read_plan_rows,
                    label_stats_delta,
                    emitted_delta_first_seq,
                    emitted_delta_last_seq,
                    hot_forward_vertices,
                });
            }
            skip_index = false;
            seed_rows.clear();
        } else {
            // Seeds apply to the first read plan that still has rows; `mem::take` consumes them once.
            let use_seeds = skip_index && !seed_rows.is_empty();
            let initial = if use_seeds {
                std::mem::take(&mut seed_rows)
            } else {
                vec![crate::plan::empty_row_for_plan(plan)]
            };
            let skip = use_seeds;
            match materialize {
                TransactionReadMaterialize::Full => {
                    last_query_rows = execute_plan_query_with_rows(
                        store,
                        plan,
                        parameters,
                        index,
                        execution.clone(),
                        initial,
                        skip,
                    )
                    .await?;
                }
                TransactionReadMaterialize::LastReadRowCountOnly => {
                    let rows = execute_plan_query_bindings_with_initial_rows(
                        store,
                        plan,
                        parameters,
                        index,
                        execution.clone(),
                        initial,
                        skip,
                    )
                    .await?;
                    last_read_row_count = rows.len();
                }
                TransactionReadMaterialize::LastReadBindingsOnly => {
                    last_read_plan_rows = execute_plan_query_bindings_with_initial_rows(
                        store,
                        plan,
                        parameters,
                        index,
                        execution.clone(),
                        initial,
                        skip,
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
        label_stats_delta,
        emitted_delta_first_seq,
        emitted_delta_last_seq,
        hot_forward_vertices,
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

/// Run pre-decoded wire plans from the router (no parse/plan on graph).
pub async fn run_wire_plans_last_read_row_count(
    store: GraphStore,
    plans: &[gleaph_gql_planner::PhysicalPlan],
    bundle_requires_write: bool,
    parameters: &BTreeMap<String, Value>,
    mode: GqlCanisterExecutionMode,
    index: Option<&dyn PropertyIndexLookup>,
    execution: GqlExecutionContext,
    seeds: Option<SeedBindingsWire>,
    mutation_id: Option<MutationId>,
) -> Result<WirePlanRunResult, GqlRunError> {
    let run = run_wire_plans(
        &store,
        plans,
        bundle_requires_write,
        parameters,
        index,
        mode,
        execution,
        seeds,
        TransactionReadMaterialize::LastReadBindingsOnly,
        mutation_id,
    )
    .await?;
    if let Some(mutation_id) = mutation_id {
        store.commit_record_completed_mutation_journal(
            mutation_id,
            run.last_read_row_count as u64,
            run.emitted_delta_first_seq,
            run.emitted_delta_last_seq,
            run.hot_forward_vertices.clone(),
        );
    }
    let rows_blob = if mode == GqlCanisterExecutionMode::CompositeQuery {
        let materialized = PlanQueryResult::try_from_plan_rows(&store, &run.last_read_plan_rows)?;
        let wire = crate::plan::ic_wire_from_plan_query_result(&materialized)
            .map_err(|e| GqlRunError::Plan(e.to_string()))?;
        Some(
            wire.encode_blob()
                .map_err(|e| GqlRunError::Plan(e.to_string()))?,
        )
    } else {
        None
    };
    Ok(WirePlanRunResult {
        row_count: run.last_read_row_count,
        rows_blob,
        hot_forward_vertices: run.hot_forward_vertices,
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
    mutation_id: Option<MutationId>,
) -> Result<WirePlanRunResult, GqlRunError> {
    let (bundle_requires_write, plans) =
        decode_plan_bundle(plan_blob).map_err(|e| GqlRunError::Plan(e.to_string()))?;
    run_wire_plans_last_read_row_count(
        store,
        &plans,
        bundle_requires_write,
        parameters,
        mode,
        index,
        execution,
        seeds,
        mutation_id,
    )
    .await
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
    use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_graph_prepared::{PreparedQueryRecord, compile_prepared_source};

    fn compile_prepared(source: &str) -> PreparedQueryRecord {
        let program = compile_prepared_source(source).expect("compile");
        let requires_write_path = classify_program(&program).requires_write_path();
        PreparedQueryRecord {
            program,
            requires_write_path,
        }
    }

    /// Index whose label-membership posting inserts always fail (compensating removes
    /// succeed), so `label_pending::flush_pending` compensates the batch to its pre-batch
    /// state, journals it for repair, and returns `IndexFlushDeferred` (ADR 0023/0024).
    struct DeferLabelFlushIndex;

    #[async_trait::async_trait(?Send)]
    impl crate::index::lookup::PropertyIndexLookup for DeferLabelFlushIndex {
        fn local_shard_id(&self) -> gleaph_graph_kernel::federation::ShardId {
            gleaph_graph_kernel::federation::ShardId::new(0)
        }
        async fn lookup_equal(
            &self,
            _: u32,
            _: Vec<u8>,
        ) -> Result<Vec<gleaph_graph_kernel::index::PostingHit>, crate::plan::PlanQueryError>
        {
            unimplemented!("reads are not exercised by this test")
        }
        async fn lookup_range(
            &self,
            _: u32,
            _: &gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Result<Vec<gleaph_graph_kernel::index::PostingHit>, crate::plan::PlanQueryError>
        {
            unimplemented!("reads are not exercised by this test")
        }
        async fn lookup_intersection(
            &self,
            _: &gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, crate::plan::PlanQueryError>
        {
            unimplemented!("reads are not exercised by this test")
        }
        async fn posting_insert_at(
            &self,
            _: gleaph_graph_kernel::federation::ShardId,
            _: u32,
            _: Vec<u8>,
            _: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }
        async fn posting_remove_at(
            &self,
            _: gleaph_graph_kernel::federation::ShardId,
            _: u32,
            _: Vec<u8>,
            _: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }
        async fn label_posting_insert_at(
            &self,
            _: gleaph_graph_kernel::federation::ShardId,
            _: u32,
            _: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Err(crate::plan::PlanQueryError::FederatedIndexCall {
                op: "label_posting_insert",
                detail: "injected label flush failure".into(),
            })
        }
        async fn label_posting_remove_at(
            &self,
            _: gleaph_graph_kernel::federation::ShardId,
            _: u32,
            _: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }
    }

    fn insert_vertex_plan(label: &str) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec![label.into()],
            properties: vec![],
        }])
    }

    #[test]
    fn emit_unique_acquires_pins_acquire_for_the_created_vertex() {
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["AcqOwner"], Vec::<(&str, Value)>::new())
            .expect("create owner vertex");
        let expected_owner = store
            .path_vertex_element_id(vid)
            .expect("owner element id")
            .to_bytes()
            .to_vec();

        let claims = vec![UniqueClaimDispatch {
            claim_ordinal: 0,
            constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(3),
            encoded_value: b"alice".to_vec(),
        }];
        let bindings = PlanMutationBindings::with_created_vertices_for_test(vec![vid]);

        emit_unique_acquires(&store, &claims, Some(42), &bindings);

        let evidence = store
            .unique_acquire_evidence(ClaimId::new(42, 0))
            .expect("Acquire receipt pinned for the claim");
        assert_eq!(evidence.effect_id, EffectId::new(42, 0));
        assert_eq!(
            evidence.owner_element_id, expected_owner,
            "owner element id must be the created vertex's canonical id"
        );
    }

    #[test]
    #[should_panic(expected = "expected exactly one created vertex")]
    fn emit_unique_acquires_traps_when_owner_is_ambiguous() {
        // The single-element admission gate guarantees exactly one created vertex; a violation here
        // is a contract breach that must trap inside the atomic section, not emit a wrong owner.
        let store = GraphStore::new();
        let claims = vec![UniqueClaimDispatch {
            claim_ordinal: 0,
            constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(3),
            encoded_value: b"alice".to_vec(),
        }];
        let bindings = PlanMutationBindings::with_created_vertices_for_test(vec![]);
        emit_unique_acquires(&store, &claims, Some(42), &bindings);
    }

    fn with_federation_routing(store: GraphStore) {
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
            }))
            .expect("set routing");
    }

    fn drain_repair_journal(store: GraphStore) {
        for (seq, _) in store.repair_journal_peek(usize::MAX) {
            store.repair_journal_remove(seq);
        }
    }

    #[test]
    fn transaction_gleaph_drain_deferred_maintenance_uses_mutation_path() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql_transaction(
            store,
            "START TRANSACTION READ WRITE CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE() YIELD remaining_queue_len COMMIT",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
            TransactionReadMaterialize::LastReadRowCountOnly,
        ))
        .expect("gleaph drain transaction");
    }

    #[test]
    fn transaction_gleaph_drain_deferred_maintenance_yield_is_returnable() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        let result = pollster::block_on(run_adhoc_gql(
            store,
            "START TRANSACTION READ WRITE CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE() YIELD remaining_queue_len RETURN remaining_queue_len AS remaining COMMIT",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("gleaph drain transaction");

        assert_eq!(
            result.rows,
            vec![BTreeMap::from([("remaining".into(), Value::Int64(0))])]
        );
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

    // Regression (ADR 0029 Phase 1 follow-up, defect #2 fixed): an inline
    // property-equality predicate on a **non-indexed** property still filters. Without
    // index stats the planner now emits a labeled NodeScan + residual PropertyFilter
    // (instead of an unexecutable IndexScan), so the equality is enforced.
    #[test]
    fn inline_property_equality_filters_without_index() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        for age in [1i64, 2] {
            pollster::block_on(run_adhoc_gql(
                store,
                &format!("INSERT (:FilterProbe {{age: {age}}})"),
                &params,
                None,
                GqlCanisterExecutionMode::Update,
                GqlExecutionContext::default(),
            ))
            .expect("insert probe vertex");
        }

        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:FilterProbe {age: 1}) RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("inline-property filtered match");
        assert_eq!(
            q.rows.len(),
            1,
            "inline property equality must filter on a non-indexed property"
        );
    }

    // Regression (defect #2 fixed, WHERE-clause form): same expectation as the inline
    // form, exercised through `WHERE n.age = 1`.
    #[test]
    fn where_property_equality_filters_without_index() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        for age in [1i64, 2] {
            pollster::block_on(run_adhoc_gql(
                store,
                &format!("INSERT (:WhereProbe {{age: {age}}})"),
                &params,
                None,
                GqlCanisterExecutionMode::Update,
                GqlExecutionContext::default(),
            ))
            .expect("insert probe vertex");
        }

        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:WhereProbe) WHERE n.age = 1 RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("WHERE-clause filtered match");
        assert_eq!(
            q.rows.len(),
            1,
            "WHERE property equality must filter on a non-indexed property"
        );
    }

    // Regression (exec defect #1, fixed): a MATCH-bound variable stays bound across a
    // following INSERT clause so a later DELETE can reference it. Before the read-phase
    // seeding fix the segment failed with `MissingVertexBinding { variable: "d" }`.
    #[test]
    fn match_binding_survives_insert_for_following_delete() {
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (:BindHub)",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("insert detached hub");

        pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (d:BindHub) INSERT (:BindOrphan) DELETE d",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("MATCH-bound variable must stay bound across INSERT for the following DELETE");

        let hub = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:BindHub) RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("query hub");
        assert_eq!(hub.rows.len(), 0, "the matched hub must be deleted");

        let orphan = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:BindOrphan) RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("query orphan");
        assert_eq!(orphan.rows.len(), 1, "the inserted orphan must persist");
    }

    #[test]
    fn wire_update_persists_label_stats_delta_and_dedupes_retry() {
        let store = GraphStore::new();
        let plan = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec!["WireTelemetryPerson".into()],
            properties: vec![],
        }]);
        let blob = encode_block_plans(&[plan], true).expect("encode plan");
        let params = BTreeMap::new();

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            None,
            Some(42),
        ))
        .expect("first wire update");

        let journal = store
            .get_mutation_journal_entry(42)
            .expect("journal entry after first update");
        assert!(journal.emitted_delta_first_seq.is_some());
        assert_eq!(
            journal.emitted_delta_first_seq,
            journal.emitted_delta_last_seq
        );
        let delta = store
            .pending_label_stats_deltas(journal.emitted_delta_first_seq.unwrap(), 10)
            .pop()
            .expect("pending delta");
        assert_eq!(delta.mutation_id, 42);
        assert_eq!(
            delta.label_stats_delta.vertex,
            vec![(
                crate::test_labels::vertex_label_id_for_name("WireTelemetryPerson"),
                1
            )]
        );

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            None,
            Some(42),
        ))
        .expect("retry wire update");
        assert_eq!(store.get_mutation_journal_entry(42), Some(journal.clone()));

        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:WireTelemetryPerson) RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("query inserted vertex");
        assert_eq!(q.rows.len(), 1);
    }

    // ADR 0024: a single-DML mutation whose post-mutation index flush is deferred to the
    // repair journal must still be recorded `Completed` (store mutation + deltas are durable
    // and the index converges via the maintenance timer), not wedged `Incomplete` forever.
    #[test]
    fn deferred_index_flush_completes_single_dml_mutation_journal() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);

        let blob =
            encode_block_plans(&[insert_vertex_plan("DeferFlushSolo")], true).expect("encode plan");
        let params = BTreeMap::new();

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            Some(&DeferLabelFlushIndex),
            GqlExecutionContext::default(),
            None,
            Some(701),
        ))
        .expect("deferred flush still completes the single-DML mutation");

        let journal = store
            .mutation_journal_entry(701)
            .expect("journal entry recorded");
        assert!(
            journal.is_completed(),
            "single-DML mutation must complete despite a repair-journaled flush"
        );
        assert!(
            !store.repair_journal_is_empty(),
            "the deferred label batch must be journaled for repair"
        );

        // Retry is idempotent: the early guard returns the cached Completed outcome.
        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            Some(&DeferLabelFlushIndex),
            GqlExecutionContext::default(),
            None,
            Some(701),
        ))
        .expect("retry of a completed mutation is idempotent");

        drain_repair_journal(store);
        store.set_federation_routing(None).expect("clear routing");
    }

    // ADR 0024: a deferred flush with unexecuted DML still ahead must NOT complete (that would
    // silently drop the later writes on idempotent replay); it errors and stays Incomplete.
    #[test]
    fn deferred_index_flush_leaves_multi_dml_mutation_incomplete() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);

        let blob = encode_block_plans(
            &[
                insert_vertex_plan("DeferFlushFirst"),
                insert_vertex_plan("DeferFlushSecond"),
            ],
            true,
        )
        .expect("encode plan");
        let params = BTreeMap::new();

        let err = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            Some(&DeferLabelFlushIndex),
            GqlExecutionContext::default(),
            None,
            Some(702),
        ))
        .expect_err("mid-bundle deferred flush with remaining DML must error");
        assert!(
            err.to_string().contains("unexecuted DML remaining"),
            "unexpected error: {err}"
        );

        let journal = store
            .mutation_journal_entry(702)
            .expect("incomplete entry recorded before the deferred flush");
        assert!(
            !journal.is_completed(),
            "a partial multi-DML bundle must stay Incomplete"
        );

        drain_repair_journal(store);
        store.set_federation_routing(None).expect("clear routing");
    }

    // ADR 0029 Phase 1: the canonical critical section commits shard-local canonical data, the
    // mutation-journal progress record, and the label-stats projection *intent* together, in one
    // message segment — independently of (and before) asynchronous index projection *delivery*
    // (`flush_pending`). Here index delivery is forced to defer to the repair journal, yet all
    // three owner-local facts are durably present.
    #[test]
    fn canonical_segment_commits_canonical_data_and_projection_intent_together() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);

        let blob = encode_block_plans(&[insert_vertex_plan("Phase1Canonical")], true)
            .expect("encode plan");
        let params = BTreeMap::new();

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            Some(&DeferLabelFlushIndex),
            GqlExecutionContext::default(),
            None,
            Some(910),
        ))
        .expect("canonical segment commits despite deferred projection delivery");

        // Mutation progress: the shard-local journal records the outcome in-segment.
        let journal = store
            .mutation_journal_entry(910)
            .expect("mutation journal entry recorded");
        assert!(
            journal.is_completed(),
            "single-DML canonical outcome must be journaled"
        );

        // Projection intent: the label-stats delta is durably logged in the same flow...
        let first_seq = journal
            .emitted_delta_first_seq
            .expect("label-stats delta intent recorded");
        let delta = store
            .pending_label_stats_deltas(first_seq, 10)
            .pop()
            .expect("pending label-stats delta");
        assert_eq!(delta.mutation_id, 910);

        // ...while projection *delivery* (index postings) was deferred to the repair journal,
        // proving canonical commit is decoupled from inter-canister projection delivery.
        assert!(
            !store.repair_journal_is_empty(),
            "deferred index delivery must be journaled for repair"
        );

        // Canonical data: the vertex itself is durably present and locally readable.
        store.set_federation_routing(None).expect("clear routing");
        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:Phase1Canonical) RETURN n",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("read back canonical vertex");
        assert_eq!(q.rows.len(), 1);

        drain_repair_journal(store);
    }

    #[test]
    fn wire_plan_seed_bindings_skip_label_intersection_prefix() {
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql::types::LabelExpr;
        use gleaph_gql_planner::plan::ProjectColumn;
        use gleaph_graph_kernel::plan_exec::SeedBindingEntry;

        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(
                ["WireSeedPerson", "WireSeedEmployee"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("vertex with both labels");
        let _person_only = store
            .insert_vertex_named(["WireSeedPerson"], Vec::<(&str, Value)>::new())
            .expect("person only");
        let local_vid = u32::try_from(u64::from(vid)).expect("local vertex id");
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("WireSeedPerson".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(Expr::new(ExprKind::Variable("n".into()))),
                    label: LabelExpr::Name("WireSeedEmployee".into()),
                    negated: false,
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let blob = encode_block_plans(&[plan], false).expect("encode plan");
        let seeds = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "n".into(),
                local_vertex_ids: vec![local_vid],
                local_edge_postings: Vec::new(),
            }],
        };
        let params = BTreeMap::new();

        let run = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::CompositeQuery,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            None,
        ))
        .expect("wire seeded label intersection");

        assert_eq!(run.row_count, 1);
        let rows_blob = run.rows_blob.expect("composite query rows");
        let wire = gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode");
        let materialized =
            crate::plan::plan_query_result_from_ic_wire(wire).expect("materialize rows");
        assert_eq!(materialized.rows.len(), 1);
        assert!(materialized.rows[0].contains_key("n"));
    }

    #[test]
    fn wire_plan_seed_bindings_apply_to_first_read_in_multi_plan_bundle() {
        use gleaph_gql::ast::{CmpOp, Expr, ExprKind};
        use gleaph_gql_planner::plan::{ProjectColumn, ScanValue};
        use gleaph_graph_kernel::plan_exec::SeedBindingEntry;

        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["WireMultiIxSeed"], [("age", Value::Uint8(5))])
            .expect("vertex");
        let local_vid = u32::try_from(u64::from(vid)).expect("local vertex id");
        let index_plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "age".into(),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".into())),
                    alias: Some("n".into()),
                }],
                distinct: false,
            },
        ]);
        let tail_plan = PhysicalPlan::from_ops(vec![PlanOp::Project {
            columns: vec![ProjectColumn {
                expr: Expr::new(ExprKind::Literal(Value::Int64(1))),
                alias: Some("x".into()),
            }],
            distinct: false,
        }]);
        let blob = encode_block_plans(&[index_plan, tail_plan], false).expect("encode bundle");
        let seeds = SeedBindingsWire {
            entries: vec![SeedBindingEntry {
                variable: "n".into(),
                local_vertex_ids: vec![local_vid],
                local_edge_postings: Vec::new(),
            }],
        };
        let params = BTreeMap::new();

        let run = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::CompositeQuery,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            None,
        ))
        .expect("multi-plan wire bundle");

        assert_eq!(run.row_count, 1);
    }

    #[test]
    fn wire_update_rejects_dml_without_mutation_id() {
        let store = GraphStore::new();
        let plan = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec!["WireMissingMutationIdPerson".into()],
            properties: vec![],
        }]);
        let blob = encode_block_plans(&[plan], true).expect("encode plan");
        let params = BTreeMap::new();

        let err = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            None,
            None,
        ))
        .expect_err("missing mutation_id should fail before DML execution");

        match err {
            GqlRunError::Plan(message) => {
                assert_eq!(message, "wire DML execution requires mutation_id");
            }
            other => panic!("expected plan error, got: {other}"),
        }
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
            GqlExecutionContext {
                caller: Some(p),
                ..Default::default()
            },
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
            GqlExecutionContext {
                caller: Some(p),
                ..Default::default()
            },
        ))
        .expect("insert");
        let q = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (n:MsgCallerOwner) RETURN n.owner AS o",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext {
                caller: Some(p),
                ..Default::default()
            },
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
            GqlExecutionContext {
                caller: Some(exec),
                ..Default::default()
            },
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
            GqlExecutionContext {
                caller: Some(p),
                ..Default::default()
            },
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
            GqlExecutionContext {
                caller: Some(p),
                ..Default::default()
            },
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
        let label_id = crate::test_labels::edge_label_id_for_name("WgtGqlRoad");
        crate::test_labels::install_test_edge_payload_profile(
            label_id,
            gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            }),
        );
        store
            .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &1u16.to_le_bytes())
            .expect("a->b");
        store
            .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &1u16.to_le_bytes())
            .expect("b->c");
        store
            .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &100u16.to_le_bytes())
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
    fn adhoc_gleaph_cost_shortest_k_returns_weighted_paths() {
        let store = GraphStore::new();
        setup_gql_weighted_graph(&store);
        let params = BTreeMap::new();
        let out = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH SHORTEST 2 (a:WgtGqlA)-[e:WgtGqlRoad]->{1,5}(c:WgtGqlC) GLEAPH.COST BY GLEAPH.WEIGHT(e) RETURN c",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("weighted shortest k");
        assert_eq!(out.rows.len(), 2);
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
