//! Parse, plan, and execute GQL against [`GraphStore`] (library / unit tests; RBAC on router).

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::pending;
use crate::plan::{
    PlanBinding, PlanMutationBindings, PlanQueryResult, PlanQueryRow, SeededMutationRow,
    execute_mutation_tail_async, execute_plan_query, execute_plan_query_bindings,
    execute_plan_query_bindings_with_initial_rows, plan_contains_gleaph_finalize_call,
    read_prefix_len,
};
use gleaph_gql::Value;
use gleaph_gql::ast::{CmpOp, Statement, StatementBlock};
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::plan::{PlanOp, ScanValue};
use gleaph_gql_planner::{PlanBuildOptions, build_statement_plan_with_options};
use gleaph_graph_kernel::entry::{ConstraintNameId, VertexLabelId};
use gleaph_graph_kernel::federation::{
    ClaimId, EffectId, ElementIdEncodingKey, UniqueEffectOp, UniqueEffectReceipt,
};
use gleaph_graph_kernel::plan_exec::{
    GqlExecutionMode as KernelGqlExecutionMode, LabelStatsDelta, MutationId, SeedBindingsWire,
    ShardEventSeq, UniqueClaimDispatch,
};
use gleaph_graph_prepared::PreparedQueryRecord;
use ic_stable_lara::VertexId;

#[cfg(feature = "canbench")]
use canbench_rs::bench_scope as canbench_scope;
use gleaph_gql_integration::path_extension::GLEAPH_PATH_EXTENSION_HANDLER;
use std::collections::BTreeMap;

#[cfg(target_family = "wasm")]
fn current_instruction_counter() -> u64 {
    ic_cdk::api::call_context_instruction_counter()
}

#[cfg(not(target_family = "wasm"))]
fn current_instruction_counter() -> u64 {
    0
}

#[cfg(all(feature = "batch-instr-log", target_family = "wasm"))]
fn log_wire_phase(entrypoint: &str, phase: &str, cost: u64) {
    let line = format!(
        "GLEAPH_WIRE_PHASE entrypoint={} phase={} cost={}",
        entrypoint, phase, cost
    );
    crate::instr_log::push(line);
}

#[cfg(all(feature = "batch-instr-log", not(target_family = "wasm")))]
#[allow(dead_code)]
#[inline]
fn log_wire_phase(_entrypoint: &str, _phase: &str, _cost: u64) {}

#[cfg(not(feature = "batch-instr-log"))]
#[inline]
fn log_wire_phase(_entrypoint: &str, _phase: &str, _cost: u64) {}

/// Opens a canbench scope when the `canbench` feature is enabled; no-op otherwise.
macro_rules! bench_scope {
    ($name:expr, $var:ident) => {
        #[cfg(feature = "canbench")]
        let $var = canbench_scope($name);
        #[cfg(not(feature = "canbench"))]
        let $var = ();
    };
}

/// Explicitly closes a canbench scope; no-op when `canbench` is disabled.
macro_rules! bench_scope_end {
    ($var:ident) => {
        #[cfg(feature = "canbench")]
        drop($var);
    };
}

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
    let seed_rows = read_phase_seed_rows(
        store,
        plan,
        parameters,
        index,
        &execution,
        router_seed,
        false,
    )
    .await?;
    let mutation_ops = &plan.ops[read_prefix_len(&plan.ops)..];
    let mutation = match seed_rows {
        None => {
            // Bare INSERT: run the mutation tail once with no seed rows.
            execute_mutation_tail_async(store, mutation_ops, &[], parameters, execution).await?
        }
        Some(rows) if rows.is_empty() => {
            // Read prefix produced zero matches; nothing to mutate.
            PlanMutationBindings::default()
        }
        Some(rows) => {
            execute_mutation_tail_async(store, mutation_ops, &rows, parameters, execution).await?
        }
    };
    Ok(mutation)
}

/// ADR 0046 Phase 2: seed rows supplied as complete-prefix candidates must still be validated
/// against canonical Graph state for the leading scan ops that the executor skips. This function
/// re-applies the semantic of each leading `NodeScan`, equality `IndexScan`, and
/// `IndexIntersection` arm against the current local vertex, without using Property Index.
///
/// The physical plan remains the single source of predicate semantics; this helper only performs
/// the checks that the skipped scan ops would otherwise have narrowed. Residual
/// `PropertyFilter`s, joins, and Cartesian products are evaluated by the normal executor.
fn revalidate_seed_rows_against_read_prefix(
    store: &GraphStore,
    plan: &gleaph_gql_planner::PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    execution: &GqlExecutionContext,
    rows: &mut Vec<PlanQueryRow>,
) -> Result<(), GqlRunError> {
    for op in plan
        .ops
        .iter()
        .take_while(|op| is_seed_skippable_anchor_op(op))
    {
        match op {
            PlanOp::NodeScan {
                variable,
                label: Some(label_ref),
                ..
            } => {
                let Some(label_id) = execution.resolved_vertex_label_id(label_ref.as_ref()) else {
                    return Err(GqlRunError::Plan(format!(
                        "resolved label id not found for {} in complete-prefix seed validation",
                        label_ref.as_ref()
                    )));
                };
                rows.retain(|row| {
                    let Some(PlanBinding::Vertex(vid)) = row.get(variable.as_ref()) else {
                        return false;
                    };
                    let Some(vertex) = store.vertex(*vid) else {
                        return false;
                    };
                    store.vertex_labels(*vid, vertex).contains(&label_id)
                });
            }
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                ..
            } if *cmp == CmpOp::Eq => {
                let Some(property_id) = execution.resolved_property_id(property.as_ref()) else {
                    return Err(GqlRunError::Plan(format!(
                        "resolved property id not found for {} in complete-prefix seed validation",
                        property.as_ref()
                    )));
                };
                let expected = resolve_scan_value_to_value(value, parameters)?;
                rows.retain(|row| {
                    let Some(PlanBinding::Vertex(vid)) = row.get(variable.as_ref()) else {
                        return false;
                    };
                    let actual = store.vertex_property(*vid, property_id);
                    actual.as_ref() == Some(&expected)
                });
            }
            PlanOp::IndexIntersection {
                variable, scans, ..
            } => {
                rows.retain(|row| {
                    let Some(PlanBinding::Vertex(vid)) = row.get(variable.as_ref()) else {
                        return false;
                    };
                    scans.iter().all(|scan| {
                        if scan.cmp != CmpOp::Eq {
                            return true;
                        }
                        let Some(property_id) =
                            execution.resolved_property_id(scan.property.as_ref())
                        else {
                            return false;
                        };
                        let Ok(expected) = resolve_scan_value_to_value(&scan.value, parameters)
                        else {
                            return false;
                        };
                        store.vertex_property(*vid, property_id).as_ref() == Some(&expected)
                    })
                });
            }
            PlanOp::NodeScan { label: None, .. } | PlanOp::EdgeIndexScan { .. } => {
                // Non-equality scans and label-less scans do not impose a check beyond existence,
                // which seed hydration already verified. Edge seeds are not produced by the current
                // multi-variable path; equality range scans fall back to ordinary execution.
            }
            PlanOp::IndexScan { cmp, .. } if *cmp != CmpOp::Eq => {
                // Non-equality index scans are not validated here; the executor handles them when
                // they are not skipped.
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_seed_skippable_anchor_op(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::NodeScan { label: Some(_), .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::EdgeIndexScan { .. }
    )
}

fn resolve_scan_value_to_value(
    value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Value, GqlRunError> {
    match value {
        ScanValue::Literal(v) => Ok(v.clone()),
        ScanValue::Parameter(name) => parameters.get(name.as_ref()).cloned().ok_or_else(|| {
            GqlRunError::Plan(format!(
                "missing parameter {} in complete-prefix seed validation",
                name.as_ref()
            ))
        }),
    }
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
    complete_prefix_rows: bool,
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
    let mut rows = execute_plan_query_bindings_with_initial_rows(
        store,
        &read_plan,
        parameters,
        index,
        execution.clone(),
        initial_rows,
        skip_leading,
    )
    .await?;

    // ADR 0046 Phase 2: when the router supplied complete rows for the entire read prefix,
    // re-validate the skipped leading anchor operators against current canonical Graph state.
    let _phase_v0 = current_instruction_counter();
    if complete_prefix_rows {
        revalidate_seed_rows_against_read_prefix(store, plan, parameters, execution, &mut rows)?;
    }
    let _phase_v1 = current_instruction_counter();
    log_wire_phase(
        "read_phase_seed_rows",
        "revalidation",
        _phase_v1.saturating_sub(_phase_v0),
    );

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
    /// A `ShardLocalGlobal` local-table claim conflicts (ADR 0030 slice 10): either two claims in
    /// this mutation collide, or the value is already owned. Detected in the all-or-nothing preflight
    /// **before** any canonical write, so the mutation aborts cleanly with no partial state.
    UniquenessViolation(String),
}

impl std::fmt::Display for GqlRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(s) => write!(f, "parse error: {s}"),
            Self::Plan(s) => write!(f, "plan error: {s}"),
            Self::Query(e) => write!(f, "{e}"),
            Self::Mutation(e) => write!(f, "{e}"),
            Self::UniquenessViolation(s) => write!(
                f,
                "{}{s}",
                gleaph_graph_kernel::federation::UNIQUENESS_VIOLATION_WIRE_PREFIX
            ),
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
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
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
    element_id_key: &ElementIdEncodingKey,
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
        .path_vertex_element_id(element_id_key, owner_vertex)
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

/// All-or-nothing preflight for `ShardLocalGlobal` local-table claims (ADR 0030 slice 10), run
/// before any canonical write. Rejects a claim that collides with another claim in the same mutation
/// or with a value already present in the local table. Returns
/// [`GqlRunError::UniquenessViolation`] on the first conflict so the mutation aborts cleanly with no
/// partial state; on `Ok` every claim is provably insertable.
fn preflight_local_unique_claims(
    store: &GraphStore,
    claims: &[UniqueClaimDispatch],
) -> Result<(), GqlRunError> {
    let mut seen: std::collections::BTreeSet<(ConstraintNameId, &[u8])> =
        std::collections::BTreeSet::new();
    for claim in claims {
        if !seen.insert((claim.constraint_id, claim.encoded_value.as_slice())) {
            return Err(GqlRunError::UniquenessViolation(format!(
                "constraint {} value claimed twice in one mutation",
                claim.constraint_id
            )));
        }
        if store.local_unique_contains(claim.constraint_id, &claim.encoded_value) {
            return Err(GqlRunError::UniquenessViolation(format!(
                "constraint {} value already exists",
                claim.constraint_id
            )));
        }
    }
    Ok(())
}

/// Inserts the preflighted `ShardLocalGlobal` claims into the local unique table (ADR 0030 slice
/// 10), all owned by the single element this segment created — mirroring [`emit_unique_acquires`]'
/// owner resolution. Runs inside the no-`await` canonical section. Any shape other than a single
/// created vertex is an admission/dispatch invariant violation and traps (rolling back the segment).
fn apply_local_unique_acquires(
    store: &GraphStore,
    element_id_key: &ElementIdEncodingKey,
    claims: &[UniqueClaimDispatch],
    mutation: &PlanMutationBindings,
) {
    let owner_vertex = match mutation.created_vertices.as_slice() {
        [vertex_id] => *vertex_id,
        created => unique_acquire_trap(&format!(
            "expected exactly one created vertex for {} local uniqueness claim(s), got {}",
            claims.len(),
            created.len()
        )),
    };
    let owner_element_id = store
        .path_vertex_element_id(element_id_key, owner_vertex)
        .map(|id| id.to_bytes().to_vec())
        .unwrap_or_else(|| {
            unique_acquire_trap("owner element id unavailable after insert (no global vertex id)")
        });
    for claim in claims {
        store.local_unique_insert(
            claim.constraint_id,
            claim.encoded_value.clone(),
            owner_element_id.clone(),
        );
    }
}

/// Pins one `Release` receipt per constrained value the segment freed (ADR 0030 slice 5b). Each
/// release was captured pre-delete with its canonical `encoded_value` and owning element id, so the
/// Router keys the same reservation and matches it by `owner_element_id`. `next_release_ordinal` is
/// the mutation-wide running `effect_ordinal` cursor: a single mutation can run several canonical
/// segments (one per DML statement), so the cursor must be carried **across** segments — restarting
/// it per segment would re-mint the same `EffectId`s and trap the outbox on a different receipt. It
/// is initialized past the mutation's `Acquire` ordinals so every effect id stays distinct and is
/// deterministic across replays. A `Release` carries no `claim_id` (the freeing mutation differs
/// from the original `Acquire`; matching is by owner).
fn emit_unique_releases(
    store: &GraphStore,
    mutation_id: Option<MutationId>,
    next_release_ordinal: &mut u32,
    mutation: &PlanMutationBindings,
) {
    let Some(mutation_id) = mutation_id else {
        unique_release_emit_trap("unique releases captured without a mutation_id");
    };
    for release in &mutation.released_unique_values {
        let effect_ordinal = *next_release_ordinal;
        *next_release_ordinal += 1;
        store.emit_unique_effect(UniqueEffectReceipt {
            effect_id: EffectId::new(mutation_id, effect_ordinal),
            claim_id: None,
            owner_element_id: release.owner_element_id.clone(),
            constraint_id: release.constraint_id,
            encoded_value: release.encoded_value.clone(),
            op: UniqueEffectOp::Release,
        });
    }
}

fn unique_release_emit_trap(message: &str) -> ! {
    let message = format!("unique-effect Release emit failed inside DML atomic section: {message}");
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::trap(&message);
    }
    #[cfg(not(target_family = "wasm"))]
    {
        panic!("{message}");
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

pub(crate) fn extend_delta_seq_range(
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

/// Delivers any queued derived vector-index mutations (ADR 0031), constructing the wasm client from
/// `vector_index_canister` routing. The vector client is not threaded through the execution path
/// (Slice 2 has no vector reads); on native builds this is a no-op unless a test queued ops, in
/// which case [`crate::index::vector_pending::flush_pending`] journals them for repair.
async fn flush_vector_pending(mutation_id: Option<u64>) -> Result<(), crate::plan::PlanQueryError> {
    #[cfg(target_family = "wasm")]
    {
        let client = crate::facade::GraphStore::new()
            .federation_routing()
            .and_then(|r| r.vector_index_canister)
            .map(
                |vector_principal| crate::index::vector_ic::IcVectorIndexClient {
                    vector_principal,
                },
            );
        let vx = client
            .as_ref()
            .map(|c| c as &dyn crate::index::vector_lookup::VectorIndexLookup);
        crate::index::vector_pending::flush_pending(vx, mutation_id).await
    }
    #[cfg(not(target_family = "wasm"))]
    {
        crate::index::vector_pending::flush_pending(None, mutation_id).await
    }
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
    let result =
        run_transaction_block_inner(store, block, parameters, index, execution, materialize).await;
    if result.is_err() {
        persist_pending_to_outbox(store, 0);
    }
    result
}

async fn run_transaction_block_inner(
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
    let mut pending_dml = false;
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
            pending_dml = true;
        } else {
            if pending_dml {
                pending::flush_all_pending(index, None).await?;
                flush_vector_pending(None).await?;
                pending_dml = false;
            }
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
    persist_pending_to_outbox(store, 0);
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
    'rows: for row in &seeds.rows {
        let mut plan_row = PlanQueryRow::new();
        for vertex in &row.vertex_bindings {
            let vertex_id = VertexId::from(vertex.local_vertex_id);
            let Some(v) = store.vertex(vertex_id) else {
                continue 'rows;
            };
            if v.is_tombstone() {
                continue 'rows;
            }
            if !vertex.required_vertex_label_ids.is_empty() {
                let labels = store.vertex_labels(vertex_id, v);
                let required: Vec<_> = vertex
                    .required_vertex_label_ids
                    .iter()
                    .copied()
                    .map(VertexLabelId::from_raw)
                    .collect();
                if !required.iter().all(|required| labels.contains(required)) {
                    continue 'rows;
                }
            }
            plan_row.insert(vertex.variable.clone(), PlanBinding::Vertex(vertex_id));
        }
        for float64 in &row.float64_bindings {
            plan_row.insert(
                float64.variable.clone(),
                PlanBinding::Value(Value::Float64(float64.value)),
            );
        }
        all_rows.push(plan_row);
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
    check_journal: bool,
) -> Result<TransactionBlockRun, GqlRunError> {
    crate::edge_inline_value_schema::set_execution_resolved_labels(
        execution.resolved_labels.clone(),
    );
    // The element-id encoding key is threaded as owned data (evaluator / materialization / canonical
    // segment), never parked in ambient thread-local state across the `await`s below.
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
        check_journal,
    )
    .await;
    crate::edge_inline_value_schema::clear_execution_resolved_labels();
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
/// inter-canister call. Index posting *delivery* happens after the successful wire-DML message via
/// the durable derived-index outbox (the asynchronous projection boundary), never inside it. The missing index parameter is the
/// structural enforcement of the "no remote call inside the critical section" invariant.
///
/// Matched-variable mutations (`MATCH ... DELETE`/`SET`/etc.) run their **read prefix in a
/// separate read phase** (with index access) *before* this segment; the resulting bindings
/// arrive as `seed_rows`, so this segment only applies the write-only mutation tail.
async fn apply_canonical_mutation_segment(
    store: &GraphStore,
    mutation_ops: &[gleaph_gql_planner::plan::PlanOp],
    seed_rows: &[SeededMutationRow],
    parameters: &BTreeMap<String, Value>,
    execution: GqlExecutionContext,
    mutation_id: Option<MutationId>,
    emitted_delta_first_seq: &mut Option<ShardEventSeq>,
    emitted_delta_last_seq: &mut Option<ShardEventSeq>,
    next_release_ordinal: &mut u32,
) -> Result<PlanMutationBindings, GqlRunError> {
    let write_journal = execution.write_journal;
    let unique_claims = execution.unique_claims.clone();
    let local_unique_claims = execution.local_unique_claims.clone();
    // Resolve the router-issued encoding key into owned data for this no-`await` canonical segment;
    // the `Acquire`/`Release` owner element ids are encoded synchronously below.
    let element_id_key =
        crate::element_id_encoding::resolve_or_host_fixture(execution.element_id_encoding_key());
    // ADR 0030 slice 10: all-or-nothing preflight of the ShardLocalGlobal claims **before** any
    // canonical write. Reject an intra-mutation duplicate or an already-present value here, so the
    // local inserts below either all apply with the canonical write or none do — no partial state.
    if !local_unique_claims.is_empty() {
        preflight_local_unique_claims(store, &local_unique_claims)?;
    }
    let _phase_t0 = current_instruction_counter();
    bench_scope!("canonical_mutation_tail", _scope_mutation_tail);
    let mutation =
        match execute_mutation_tail_async(store, mutation_ops, seed_rows, parameters, execution)
            .await
        {
            Ok(mutation) => mutation,
            Err(error) => trap_wire_mutation_failure(error),
        };
    bench_scope_end!(_scope_mutation_tail);
    let _phase_t1 = current_instruction_counter();
    log_wire_phase(
        "apply_canonical_mutation_segment",
        "execute_mutation_tail_async",
        _phase_t1.saturating_sub(_phase_t0),
    );
    // ADR 0030 slice 5: pin the cross-shard uniqueness `Acquire` receipts for the element created
    // in this segment. This runs inside the same no-`await` canonical section as the write above, so
    // the receipts commit (or roll back on trap) atomically with the canonical mutation.
    bench_scope!(
        "canonical_uniqueness_bookkeeping",
        _scope_unique_bookkeeping
    );
    if !unique_claims.is_empty() {
        emit_unique_acquires(
            store,
            &element_id_key,
            &unique_claims,
            mutation_id,
            &mutation,
        );
    }
    // ADR 0030 slice 10: insert the preflighted ShardLocalGlobal claims into the local unique table,
    // owned by the element this segment created, inside the same no-`await` section as the canonical
    // write (so they commit or roll back atomically with it). The preflight above proved every claim
    // is clean, so these inserts cannot collide.
    if !local_unique_claims.is_empty() {
        apply_local_unique_acquires(store, &element_id_key, &local_unique_claims, &mutation);
    }
    // ADR 0030 slice 5b: pin one `Release` receipt per constrained value this segment freed
    // (captured pre-delete in `mutation.released_unique_values`), in the same atomic section. The
    // `effect_ordinal` cursor is carried across the mutation's segments so multi-statement DELETEs
    // never re-mint an `EffectId`.
    if !mutation.released_unique_values.is_empty() {
        emit_unique_releases(store, mutation_id, next_release_ordinal, &mutation);
    }
    // ADR 0030 slice 10: free `ShardLocalGlobal` values directly in this shard's local unique table,
    // owner-matched so a value already reclaimed by another element is never wrongly removed. Runs in
    // the same no-`await` atomic section, so the canonical delete and the local free commit together.
    if !mutation.released_local_unique_values.is_empty() {
        for release in &mutation.released_local_unique_values {
            store.local_unique_remove_if_owner(
                release.constraint_id,
                &release.encoded_value,
                &release.owner_element_id,
            );
        }
    }
    bench_scope_end!(_scope_unique_bookkeeping);
    let has_delta = !mutation.label_stats_delta.vertex.is_empty()
        || !mutation.label_stats_delta.edge.is_empty();
    if let Some(mutation_id) = mutation_id
        && has_delta
    {
        bench_scope!("canonical_label_stats_delta_append", _scope_label_stats);
        let event = store
            .commit_append_label_stats_delta(mutation_id, mutation.label_stats_delta.clone())
            .map_err(GqlRunError::Plan)?;
        extend_delta_seq_range(
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            event.shard_event_seq,
        );
        bench_scope_end!(_scope_label_stats);
    }
    if let Some(mutation_id) = mutation_id
        && write_journal
    {
        bench_scope!(
            "canonical_incomplete_journal_write",
            _scope_incomplete_journal
        );
        store.commit_record_incomplete_mutation_journal(
            mutation_id,
            *emitted_delta_first_seq,
            *emitted_delta_last_seq,
        );
        bench_scope_end!(_scope_incomplete_journal);
    }
    let _phase_t2 = current_instruction_counter();
    log_wire_phase(
        "apply_canonical_mutation_segment",
        "post_write_bookkeeping",
        _phase_t2.saturating_sub(_phase_t1),
    );
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
    check_journal: bool,
) -> Result<TransactionBlockRun, GqlRunError> {
    if check_journal
        && let Some(mutation_id) = mutation_id
        && let Some(journal) = store.mutation_journal_entry(mutation_id)
    {
        if journal.is_completed() {
            return Ok(TransactionBlockRun {
                last_query_rows: PlanQueryResult::default(),
                last_read_row_count: journal.row_count() as usize,
                last_read_plan_rows: Vec::new(),
                label_stats_delta: LabelStatsDelta::default(),
                emitted_delta_first_seq: journal.emitted_delta_first_seq(),
                emitted_delta_last_seq: journal.emitted_delta_last_seq(),
                hot_forward_vertices: journal.hot_forward_vertices().to_vec().clone(),
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
    // ADR 0030 slice 5b: the mutation-wide `Release` `effect_ordinal` cursor, carried across every
    // canonical segment so a multi-statement DELETE/REMOVE never re-mints an `EffectId`. Starts past
    // the mutation's `Acquire` ordinals (which occupy `0..unique_claims.len()`).
    let mut next_unique_release_ordinal = execution.unique_claims.len() as u32;

    let (mut seed_rows, mut skip_index, mut complete_prefix_rows) = if let Some(ref s) = seeds {
        let (rows, skip) = seed_initial_rows(store, s)?;
        (rows, skip, s.complete_prefix_rows)
    } else {
        (Vec::new(), false, false)
    };
    // Tracks whether the router's seed relation has not yet been consumed by the first read plan.
    // This lets an explicitly empty seed relation (zero rows) still drive the first read plan
    // with skip-leading-scan semantics, while later plans in the bundle fall back to a synthetic
    // empty row as before (ADR 0034 Slice 6 empty-hit dispatch).
    let mut seeds_remaining = seeds.is_some();

    for plan in plans {
        if plan_needs_mutation_executor(plan) {
            // ADR 0029: read phase (with index) binds matched variables; consumes the router's
            // leading-anchor seed rows once, just like a read-only plan would.
            // ADR 0046 Phase 1: when the router supplied complete rows for the entire read prefix,
            // skip the read phase entirely and use the seed rows as mutation inputs.
            let _phase_r0 = current_instruction_counter();
            bench_scope!("canonical_read_phase_seed_rows", _scope_read_phase);
            let router_seed = (skip_index && !seed_rows.is_empty())
                .then(|| (std::mem::take(&mut seed_rows), true));
            let mutation_seed_rows = read_phase_seed_rows(
                store,
                plan,
                parameters,
                index,
                &execution,
                router_seed,
                complete_prefix_rows,
            )
            .await?;
            bench_scope_end!(_scope_read_phase);
            let _phase_r1 = current_instruction_counter();
            log_wire_phase(
                "run_wire_plans_inner",
                "read_phase_seed_rows",
                _phase_r1.saturating_sub(_phase_r0),
            );
            // ADR 0029 §1: shard-local canonical critical section (no inter-canister call).
            let _phase_m0 = current_instruction_counter();
            let mutation = match mutation_seed_rows {
                None => {
                    // Bare INSERT: run the mutation tail once with no seed rows.
                    apply_canonical_mutation_segment(
                        store,
                        &plan.ops[read_prefix_len(&plan.ops)..],
                        &[],
                        parameters,
                        execution.clone(),
                        mutation_id,
                        &mut emitted_delta_first_seq,
                        &mut emitted_delta_last_seq,
                        &mut next_unique_release_ordinal,
                    )
                    .await?
                }
                Some(rows) if rows.is_empty() => {
                    // Read prefix produced zero matches; nothing to mutate.
                    PlanMutationBindings::default()
                }
                Some(rows) => {
                    apply_canonical_mutation_segment(
                        store,
                        &plan.ops[read_prefix_len(&plan.ops)..],
                        &rows,
                        parameters,
                        execution.clone(),
                        mutation_id,
                        &mut emitted_delta_first_seq,
                        &mut emitted_delta_last_seq,
                        &mut next_unique_release_ordinal,
                    )
                    .await?
                }
            };
            let _phase_m1 = current_instruction_counter();
            log_wire_phase(
                "run_wire_plans_inner",
                "apply_canonical_mutation_segment",
                _phase_m1.saturating_sub(_phase_m0),
            );
            merge_label_stats_delta(&mut label_stats_delta, mutation.label_stats_delta);
            merge_hot_forward_vertices(&mut hot_forward_vertices, &mutation.hot_forward_vertices);
            record_mutation_procedure_rows(
                materialize,
                &mutation.procedure_rows,
                &mut last_query_rows,
                &mut last_read_row_count,
                &mut last_read_plan_rows,
            );
            skip_index = false;
            seed_rows.clear();
            complete_prefix_rows = false;
            seeds_remaining = false;
        } else {
            // Seeds apply to the first read plan that consumes them; `mem::take` consumes them once.
            // An explicitly empty seed relation must still drive that first plan with skip-leading-
            // scan semantics so aggregate `count(*)` over zero seed rows returns 0 (ADR 0034 Slice 6).
            // After consumption, subsequent plans fall back to a synthetic empty row as before.
            let use_seeds = if seeds_remaining {
                seeds_remaining = false;
                skip_index
            } else {
                skip_index && !seed_rows.is_empty()
            };
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
    let element_id_key =
        crate::element_id_encoding::resolve_or_host_fixture(execution.element_id_encoding_key());
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
        rows: crate::plan::materialize_plan_rows(store, &element_id_key, &rows)?,
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
    check_journal: bool,
    write_journal: bool,
) -> Result<WirePlanRunResult, GqlRunError> {
    let element_id_key =
        crate::element_id_encoding::resolve_or_host_fixture(execution.element_id_encoding_key());
    let _phase_t0 = current_instruction_counter();
    let run_result = run_wire_plans(
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
        check_journal,
    )
    .await;
    let _phase_t1 = current_instruction_counter();
    log_wire_phase(
        "run_wire_plans_last_read_row_count",
        "run_wire_plans",
        _phase_t1.saturating_sub(_phase_t0),
    );
    if let Some(mutation_id) = mutation_id {
        bench_scope!("canonical_outbox_persist", _scope_outbox_persist);
        persist_pending_to_outbox(&store, mutation_id);
    }
    let _phase_t2 = current_instruction_counter();
    log_wire_phase(
        "run_wire_plans_last_read_row_count",
        "outbox_persist",
        _phase_t2.saturating_sub(_phase_t1),
    );
    let run = run_result?;
    if write_journal && let Some(mutation_id) = mutation_id {
        bench_scope!("canonical_journal_commit", _scope_journal_commit);
        store.commit_record_completed_mutation_journal(
            mutation_id,
            run.last_read_row_count as u64,
            run.emitted_delta_first_seq,
            run.emitted_delta_last_seq,
            run.hot_forward_vertices.clone(),
        );
    }
    let _phase_t3 = current_instruction_counter();
    log_wire_phase(
        "run_wire_plans_last_read_row_count",
        "journal_commit",
        _phase_t3.saturating_sub(_phase_t2),
    );
    let rows_blob = if mode == GqlCanisterExecutionMode::CompositeQuery {
        bench_scope!("canonical_result_encode", _scope_result_encode);
        let materialized =
            PlanQueryResult::try_from_plan_rows(&store, &element_id_key, &run.last_read_plan_rows)?;
        let wire = crate::plan::ic_wire_from_plan_query_result(&materialized)
            .map_err(|e| GqlRunError::Plan(e.to_string()))?;
        Some(
            wire.encode_blob()
                .map_err(|e| GqlRunError::Plan(e.to_string()))?,
        )
    } else {
        None
    };
    let _phase_t4 = current_instruction_counter();
    log_wire_phase(
        "run_wire_plans_last_read_row_count",
        "encode_rows",
        _phase_t4.saturating_sub(_phase_t3),
    );
    Ok(WirePlanRunResult {
        row_count: run.last_read_row_count,
        rows_blob,
        hot_forward_vertices: run.hot_forward_vertices,
        emitted_delta_first_seq: run.emitted_delta_first_seq,
        emitted_delta_last_seq: run.emitted_delta_last_seq,
    })
}

fn persist_pending_to_outbox(store: &GraphStore, mutation_id: u64) {
    let mut outbox_ops = pending::take_pending_as_outbox();
    outbox_ops.extend(crate::index::vector_pending::take_pending_as_outbox());
    if outbox_ops.is_empty() {
        return;
    }
    store.derived_index_outbox_append(mutation_id, outbox_ops);
    crate::facade::maintenance_timer::arm_if_needed();
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
    bench_scope!("canonical_plan_decode", _scope_plan_decode);
    let cached_bundle = crate::index::plan_cache::decode_plan_bundle_cached(plan_blob)
        .map_err(|e| GqlRunError::Plan(e.to_string()))?;
    bench_scope_end!(_scope_plan_decode);
    run_wire_plans_last_read_row_count(
        store,
        &cached_bundle.plans,
        cached_bundle.requires_write_path,
        parameters,
        mode,
        index,
        execution,
        seeds,
        mutation_id,
        true,
        true,
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

    fn insert_vertex_plan(label: &str) -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec![label.into()],
            properties: vec![],
        }])
    }

    /// ADR 0044 scalar replay: a completed scalar journal entry lets a second call with the same
    /// mutation id return the previously committed row count without re-executing the mutation.
    /// The incomplete journal is omitted for single-message scalar execution, but the complete
    /// journal remains the durable idempotency record.
    #[test]
    fn scalar_completed_journal_replay_returns_count_without_re_executing() {
        let store = GraphStore::new();
        let plan = insert_vertex_plan("ReplayTestVertex");
        let blob = encode_block_plans(std::slice::from_ref(&plan), true).expect("encode plan");
        let params = BTreeMap::new();
        let mut execution = GqlExecutionContext::default();
        execution.write_journal = false;

        let first = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            execution.clone(),
            None,
            Some(42),
        ))
        .expect("first scalar mutation");
        assert!(
            store.mutation_journal_entry(42).is_some(),
            "completed journal must be written"
        );
        let journal = store.mutation_journal_entry(42).unwrap();
        assert!(
            journal.is_completed(),
            "scalar journal must be marked complete"
        );
        let initial_vertex_count = GraphStore::new().vertex_count();
        assert!(
            initial_vertex_count > 0.into(),
            "mutation must create at least one vertex"
        );

        let second = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            execution,
            None,
            Some(42),
        ))
        .expect("replay from completed journal");
        assert_eq!(
            second.row_count, first.row_count,
            "replay must return the same row count"
        );
        // The vertex count must remain unchanged, proving the mutation was not re-executed.
        assert_eq!(
            GraphStore::new().vertex_count(),
            initial_vertex_count,
            "completed-journal replay must not re-execute the mutation"
        );
    }

    fn local_claim(constraint: u16, value: &[u8]) -> UniqueClaimDispatch {
        UniqueClaimDispatch {
            claim_ordinal: 0,
            constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(constraint),
            encoded_value: value.to_vec(),
        }
    }

    /// ADR 0030 slice 10 all-or-nothing acquire preflight: a fully-absent claim set passes, so the
    /// canonical write and the local inserts may proceed together.
    #[test]
    fn preflight_local_unique_claims_accepts_a_clean_set() {
        let store = GraphStore::new();
        let claims = vec![local_claim(60, b"a"), local_claim(60, b"b")];
        assert!(preflight_local_unique_claims(&store, &claims).is_ok());
    }

    /// Two claims in the same mutation collide: rejected before any write, so no partial local state.
    #[test]
    fn preflight_local_unique_claims_rejects_intra_mutation_duplicate() {
        let store = GraphStore::new();
        let claims = vec![local_claim(61, b"dup"), local_claim(61, b"dup")];
        assert!(matches!(
            preflight_local_unique_claims(&store, &claims),
            Err(GqlRunError::UniquenessViolation(_))
        ));
    }

    /// A claim whose value is already owned in the local table is rejected (graph-wide uniqueness).
    #[test]
    fn preflight_local_unique_claims_rejects_value_already_present() {
        let store = GraphStore::new();
        let constraint = gleaph_graph_kernel::entry::ConstraintNameId::from_raw(62);
        store.local_unique_insert(constraint, b"taken".to_vec(), vec![1u8; 8]);
        let claims = vec![local_claim(62, b"taken")];
        assert!(matches!(
            preflight_local_unique_claims(&store, &claims),
            Err(GqlRunError::UniquenessViolation(_))
        ));
    }

    #[test]
    fn emit_unique_acquires_pins_acquire_for_the_created_vertex() {
        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["AcqOwner"], Vec::<(&str, Value)>::new())
            .expect("create owner vertex");
        let expected_owner = store
            .path_vertex_element_id(
                &gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
                vid,
            )
            .expect("owner element id")
            .to_bytes()
            .to_vec();

        let claims = vec![UniqueClaimDispatch {
            claim_ordinal: 0,
            constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(3),
            encoded_value: b"alice".to_vec(),
        }];
        let bindings = PlanMutationBindings::with_created_vertices_for_test(vec![vid]);

        emit_unique_acquires(
            &store,
            &gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
            &claims,
            Some(42),
            &bindings,
        );

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
    fn emit_unique_releases_pins_release_per_freed_value() {
        let store = GraphStore::new();
        let owner = vec![7u8; 8];
        let releases = vec![
            crate::plan::PendingUniqueRelease {
                constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(3),
                encoded_value: b"alice".to_vec(),
                owner_element_id: owner.clone(),
            },
            crate::plan::PendingUniqueRelease {
                constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(4),
                encoded_value: b"alias".to_vec(),
                owner_element_id: owner.clone(),
            },
        ];
        let bindings = PlanMutationBindings::with_released_unique_values_for_test(releases);

        // No Acquire claims in this (release-only) mutation, so the cursor starts at 0.
        let mut next_release_ordinal = 0u32;
        emit_unique_releases(&store, Some(70), &mut next_release_ordinal, &bindings);
        assert_eq!(
            next_release_ordinal, 2,
            "cursor advances once per freed value"
        );

        let effects = store.unique_release_effects_page(70, None, 100);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].effect_id, EffectId::new(70, 0));
        assert_eq!(effects[0].op, UniqueEffectOp::Release);
        assert_eq!(effects[0].claim_id, None, "a Release carries no claim_id");
        assert_eq!(effects[0].owner_element_id, owner);
        assert_eq!(effects[0].encoded_value, b"alice");
        assert_eq!(effects[1].effect_id, EffectId::new(70, 1));
        assert_eq!(effects[1].encoded_value, b"alias");
    }

    #[test]
    fn emit_unique_releases_carries_ordinal_cursor_across_segments() {
        // A multi-statement DELETE runs several canonical segments under one mutation_id. The cursor
        // is carried across them, so the second segment's releases never re-mint an EffectId. The
        // cursor is seeded past any Acquire ordinals (here a single acquire occupies ordinal 0).
        let store = GraphStore::new();
        let seg1 = PlanMutationBindings::with_released_unique_values_for_test(vec![
            crate::plan::PendingUniqueRelease {
                constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(3),
                encoded_value: b"old-a".to_vec(),
                owner_element_id: vec![1u8; 8],
            },
        ]);
        let seg2 = PlanMutationBindings::with_released_unique_values_for_test(vec![
            crate::plan::PendingUniqueRelease {
                constraint_id: gleaph_graph_kernel::entry::ConstraintNameId::from_raw(3),
                encoded_value: b"old-b".to_vec(),
                owner_element_id: vec![2u8; 8],
            },
        ]);

        // Seed the cursor past one acquire ordinal, then run two segments.
        let mut next_release_ordinal = 1u32;
        emit_unique_releases(&store, Some(71), &mut next_release_ordinal, &seg1);
        emit_unique_releases(&store, Some(71), &mut next_release_ordinal, &seg2);
        assert_eq!(next_release_ordinal, 3);

        let effects = store.unique_release_effects_page(71, None, 100);
        assert_eq!(effects.len(), 2);
        assert_eq!(
            effects[0].effect_id,
            EffectId::new(71, 1),
            "first release skips the acquire ordinal (0)"
        );
        assert_eq!(
            effects[1].effect_id,
            EffectId::new(71, 2),
            "second segment's release continues the cursor instead of restarting"
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
        emit_unique_acquires(
            &store,
            &gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
            &claims,
            Some(42),
            &bindings,
        );
    }

    fn with_federation_routing(store: GraphStore) {
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_index_canister: None,
            }))
            .expect("set routing");
    }

    fn drain_repair_journal(store: GraphStore) {
        for (seq, _) in store.repair_journal_peek(usize::MAX) {
            store.repair_journal_remove(seq);
        }
    }

    fn drain_derived_index_outbox(store: GraphStore) {
        for (seq, _) in store.derived_index_outbox_peek(usize::MAX) {
            store.derived_index_outbox_remove(seq);
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

    #[test]
    fn adhoc_dml_persists_derived_index_work_in_outbox() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);
        drain_derived_index_outbox(store);

        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (:AdhocOutbox)",
            &BTreeMap::new(),
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("ad-hoc DML");

        assert!(!store.derived_index_outbox_is_empty());
        assert!(store.repair_journal_is_empty());
        drain_derived_index_outbox(store);
        store.set_federation_routing(None).expect("clear routing");
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

    // Helpers for the NEXT edge-endpoint binding regressions below.
    fn plan_block(gql: &str) -> gleaph_gql_planner::PhysicalPlan {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        let program = parser::parse(gql).expect("parse");
        let block = program
            .transaction_activity
            .as_ref()
            .expect("transaction")
            .body
            .as_ref()
            .expect("body");
        build_block_plan_with_schema(block, None, &NoSchema).expect("plan block")
    }

    fn insert_two_bind_next_users(store: GraphStore, id1: &str, id2: &str) {
        let params = BTreeMap::new();
        let gql = format!(
            "INSERT (:BindNextUser {{id: '{}'}}), (:BindNextUser {{id: '{}'}})",
            id1, id2
        );
        pollster::block_on(run_adhoc_gql(
            store,
            &gql,
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("seed users");
    }

    fn assert_one_bind_next_edge(store: GraphStore, expected_src: &str, expected_dst: &str) {
        let params = BTreeMap::new();
        let result = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (a:BindNextUser)-[e:BIND_NEXT_FOLLOWS]->(b:BindNextUser) RETURN a.id AS aid, b.id AS bid",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("traverse edge");
        assert_eq!(
            result.rows.len(),
            1,
            "exactly one directed edge should connect the two matched users"
        );
        let row = &result.rows[0];
        assert_eq!(
            row.get("aid"),
            Some(&Value::Text(expected_src.into())),
            "source endpoint must be the matched vertex"
        );
        assert_eq!(
            row.get("bid"),
            Some(&Value::Text(expected_dst.into())),
            "destination endpoint must be the matched vertex"
        );
    }

    // Regression (exec defect #2): a MATCH-bound vertex stays bound through RETURN / NEXT
    // so a following INSERT edge can reference it.  The native Graph execution path is
    // exercised here by building a full block plan (the same shape Router produces) and
    // running it through the canonical read-prefix + mutation-tail executor.
    #[test]
    fn block_match_next_insert_edge_keeps_endpoints() {
        let store = GraphStore::new();
        insert_two_bind_next_users(store, "alice", "bob");

        let plan = plan_block(
            "MATCH (a:BindNextUser {id: 'alice'}), (b:BindNextUser {id: 'bob'}) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(b)",
        );
        let params = BTreeMap::new();
        pollster::block_on(execute_dml_plan_async(
            &store,
            &plan,
            &params,
            None,
            GqlExecutionContext::default(),
            None,
        ))
        .expect("MATCH/RETURN/NEXT INSERT edge must preserve endpoint bindings");

        assert_one_bind_next_edge(store, "alice", "bob");
    }

    // Wire-equivalent regression: encode the same block plan as the Router would and run it
    // through the wire replay path.  This proves endpoint identity survives plan encoding,
    // bundle decoding, and the canonical mutation segment.
    #[test]
    fn wire_block_match_next_insert_edge_keeps_endpoints() {
        let store = GraphStore::new();
        insert_two_bind_next_users(store, "alice", "bob");

        let plan = plan_block(
            "MATCH (a:BindNextUser {id: 'alice'}), (b:BindNextUser {id: 'bob'}) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(b)",
        );
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
            Some(1),
        ))
        .expect("wire update must preserve endpoint bindings");

        assert_one_bind_next_edge(store, "alice", "bob");
    }

    // Shared-source regression: the same previously matched source vertex can be bound in two
    // separate NEXT INSERT executions without duplicate source creation or endpoint loss.
    #[test]
    fn block_match_next_insert_edge_shares_source() {
        let store = GraphStore::new();
        // Seed alice, bob, and carol.
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (:BindNextUser {id: 'alice'}), (:BindNextUser {id: 'bob'}), (:BindNextUser {id: 'carol'})",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .expect("seed three users");

        let plan_bob = plan_block(
            "MATCH (a:BindNextUser {id: 'alice'}), (b:BindNextUser {id: 'bob'}) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(b)",
        );
        let plan_carol = plan_block(
            "MATCH (a:BindNextUser {id: 'alice'}), (c:BindNextUser {id: 'carol'}) RETURN a NEXT INSERT (a)-[:BIND_NEXT_FOLLOWS]->(c)",
        );

        for plan in [&plan_bob, &plan_carol] {
            pollster::block_on(execute_dml_plan_async(
                &store,
                plan,
                &params,
                None,
                GqlExecutionContext::default(),
                None,
            ))
            .expect("shared-source NEXT INSERT must preserve endpoint bindings");
        }

        let result = pollster::block_on(run_adhoc_gql(
            store,
            "MATCH (a:BindNextUser {id: 'alice'})-[e:BIND_NEXT_FOLLOWS]->(b:BindNextUser) RETURN b.id AS bid ORDER BY bid",
            &params,
            None,
            GqlCanisterExecutionMode::CompositeQuery,
            GqlExecutionContext::default(),
        ))
        .expect("traverse shared-source edges");
        assert_eq!(result.rows.len(), 2, "alice should have two outgoing edges");
        assert_eq!(result.rows[0].get("bid"), Some(&Value::Text("bob".into())));
        assert_eq!(
            result.rows[1].get("bid"),
            Some(&Value::Text("carol".into()))
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
        assert!(journal.emitted_delta_first_seq().is_some());
        assert_eq!(
            journal.emitted_delta_first_seq(),
            journal.emitted_delta_last_seq()
        );
        let delta = store
            .pending_label_stats_deltas(journal.emitted_delta_first_seq().unwrap(), 10)
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

    // Plan 0088: a single-DML mutation persists derived-index work in the durable outbox and
    // completes without issuing an inter-canister index flush in the mutation message.
    #[test]
    fn wire_dml_completes_with_durable_outbox_delivery() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);
        drain_derived_index_outbox(store);

        let blob =
            encode_block_plans(&[insert_vertex_plan("DeferFlushSolo")], true).expect("encode plan");
        let params = BTreeMap::new();

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
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
        assert!(store.repair_journal_is_empty());
        assert!(!store.derived_index_outbox_is_empty());

        // Retry is idempotent: the early guard returns the cached Completed outcome.
        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            None,
            Some(701),
        ))
        .expect("retry of a completed mutation is idempotent");

        drain_repair_journal(store);
        drain_derived_index_outbox(store);
        store.set_federation_routing(None).expect("clear routing");
    }

    // Plan 0088: multiple DML statements share one durable outbox handoff and complete together.
    #[test]
    fn multi_dml_mutation_persists_all_outbox_operations() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);
        drain_derived_index_outbox(store);

        let blob = encode_block_plans(
            &[
                insert_vertex_plan("DeferFlushFirst"),
                insert_vertex_plan("DeferFlushSecond"),
            ],
            true,
        )
        .expect("encode plan");
        let params = BTreeMap::new();

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            None,
            Some(702),
        ))
        .expect("all DML statements complete before outbox delivery");

        let journal = store
            .mutation_journal_entry(702)
            .expect("completed entry recorded after the outbox append");
        assert!(
            journal.is_completed(),
            "the multi-DML bundle must complete once canonical writes and outbox entries commit"
        );

        drain_repair_journal(store);
        assert!(!store.derived_index_outbox_is_empty());
        drain_derived_index_outbox(store);
        store.set_federation_routing(None).expect("clear routing");
    }

    // ADR 0029 Phase 1: the canonical critical section commits shard-local canonical data, the
    // mutation-journal progress record, and the label-stats projection *intent* together, in one
    // message segment — independently of (and before) asynchronous index projection *delivery*
    // (the durable derived-index outbox). Here index delivery is deferred to maintenance, yet all
    // three owner-local facts are durably present.
    #[test]
    fn canonical_segment_commits_canonical_data_and_projection_intent_together() {
        let store = GraphStore::new();
        with_federation_routing(store);
        drain_repair_journal(store);
        drain_derived_index_outbox(store);

        let blob = encode_block_plans(&[insert_vertex_plan("Phase1Canonical")], true)
            .expect("encode plan");
        let params = BTreeMap::new();

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &params,
            GqlCanisterExecutionMode::Update,
            None,
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
            .emitted_delta_first_seq()
            .expect("label-stats delta intent recorded");
        let delta = store
            .pending_label_stats_deltas(first_seq, 10)
            .pop()
            .expect("pending label-stats delta");
        assert_eq!(delta.mutation_id, 910);

        // ...while projection delivery (index postings) was deferred to the durable outbox,
        // proving canonical commit is decoupled from inter-canister projection delivery.
        assert!(store.repair_journal_is_empty());
        assert!(!store.derived_index_outbox_is_empty());

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
        drain_derived_index_outbox(store);
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
            rows: Vec::new(),
            complete_prefix_rows: false,
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
            rows: Vec::new(),
            complete_prefix_rows: false,
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
    fn wire_plan_row_shaped_seeds_hydrate_vertex_and_distance_alias() {
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql_planner::plan::{
            ProjectColumn, SearchOutputKind, SearchOutputPlan, SearchProviderPlan,
        };
        use gleaph_graph_kernel::plan_exec::{SeedFloat64Binding, SeedRowWire, SeedVertexBinding};

        let store = GraphStore::new();
        let vid = store
            .insert_vertex_named(["RowSeedDocument"], Vec::<(&str, Value)>::new())
            .expect("vertex");
        let local_vid = u32::try_from(u64::from(vid)).expect("local vertex id");

        let full_plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: Some("RowSeedDocument".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: Expr::var("query"),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("d".into())),
                        alias: Some("d".into()),
                    },
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("distance".into())),
                        alias: Some("distance".into()),
                    },
                ],
                distinct: false,
            },
        ]);
        let stripped_plan = PhysicalPlan {
            ops: full_plan.ops[2..].to_vec(),
            diagnostics: full_plan.diagnostics,
            annotations: full_plan.annotations,
            output: full_plan.output,
            binding_layout: full_plan.binding_layout,
        };
        let blob = encode_block_plans(&[stripped_plan], false).expect("encode stripped plan");
        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "d".into(),
                    local_vertex_id: local_vid,
                    required_vertex_label_ids: Vec::new(),
                }],
                float64_bindings: vec![SeedFloat64Binding {
                    variable: "distance".into(),
                    value: 1.25,
                }],
            }],
            complete_prefix_rows: false,
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
        .expect("wire row seed hydration");

        assert_eq!(run.row_count, 1);
        let rows_blob = run.rows_blob.expect("composite query rows");
        let wire = gleaph_gql_ic::IcWirePlanQueryResult::decode_blob(&rows_blob).expect("decode");
        let materialized =
            crate::plan::plan_query_result_from_ic_wire(wire).expect("materialize rows");
        assert_eq!(materialized.rows.len(), 1);
        let row = &materialized.rows[0];
        assert!(row.contains_key("d"));
        assert_eq!(
            row.get("distance"),
            Some(&Value::Float64(1.25)),
            "distance alias must be hydrated from row-shaped seed"
        );
    }

    #[test]
    fn wire_plan_row_shaped_seed_skips_vertex_with_missing_required_label() {
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql_planner::plan::{
            ProjectColumn, SearchOutputKind, SearchOutputPlan, SearchProviderPlan,
        };
        use gleaph_graph_kernel::plan_exec::{SeedFloat64Binding, SeedRowWire, SeedVertexBinding};

        let store = GraphStore::new();
        let matching_vid = store
            .insert_vertex_named(["RowSeedDoc"], Vec::<(&str, Value)>::new())
            .expect("vertex");
        let other_vid = store
            .insert_vertex_named(["RowSeedOther"], Vec::<(&str, Value)>::new())
            .expect("other vertex");
        let matching_local = u32::try_from(u64::from(matching_vid)).expect("local vertex id");
        let other_local = u32::try_from(u64::from(other_vid)).expect("local vertex id");
        let required_label_id = crate::test_labels::vertex_label_id_for_name("RowSeedDoc");

        let full_plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: Some("RowSeedDoc".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: Expr::var("query"),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("d".into())),
                    alias: Some("d".into()),
                }],
                distinct: false,
            },
        ]);
        let stripped_plan = PhysicalPlan {
            ops: full_plan.ops[2..].to_vec(),
            diagnostics: full_plan.diagnostics,
            annotations: full_plan.annotations,
            output: full_plan.output,
            binding_layout: full_plan.binding_layout,
        };
        let blob = encode_block_plans(&[stripped_plan], false).expect("encode stripped plan");
        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![
                SeedRowWire {
                    vertex_bindings: vec![SeedVertexBinding {
                        variable: "d".into(),
                        local_vertex_id: matching_local,
                        required_vertex_label_ids: vec![required_label_id.raw()],
                    }],
                    float64_bindings: vec![SeedFloat64Binding {
                        variable: "distance".into(),
                        value: 0.0,
                    }],
                },
                SeedRowWire {
                    vertex_bindings: vec![SeedVertexBinding {
                        variable: "d".into(),
                        local_vertex_id: other_local,
                        required_vertex_label_ids: vec![required_label_id.raw()],
                    }],
                    float64_bindings: vec![SeedFloat64Binding {
                        variable: "distance".into(),
                        value: 1.0,
                    }],
                },
            ],
            complete_prefix_rows: false,
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
        .expect("wire row seed label enforcement");

        assert_eq!(run.row_count, 1);
    }

    #[test]
    fn wire_plan_row_shaped_seed_skips_missing_and_tombstoned_vertices() {
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql_planner::plan::{
            ProjectColumn, SearchOutputKind, SearchOutputPlan, SearchProviderPlan,
        };
        use gleaph_graph_kernel::plan_exec::{SeedFloat64Binding, SeedRowWire, SeedVertexBinding};

        let store = GraphStore::new();
        let present_vid = store
            .insert_vertex_named(["RowSeedPresent"], Vec::<(&str, Value)>::new())
            .expect("present vertex");
        let tombstoned_vid = store
            .insert_vertex_named(["RowSeedTombstoned"], Vec::<(&str, Value)>::new())
            .expect("tombstoned vertex");
        store
            .delete_vertex(tombstoned_vid)
            .expect("tombstone vertex");

        let present_local = u32::try_from(u64::from(present_vid)).expect("local vertex id");
        let tombstoned_local = u32::try_from(u64::from(tombstoned_vid)).expect("local vertex id");

        let full_plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: Some("RowSeedPresent".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: Expr::var("query"),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("d".into())),
                    alias: Some("d".into()),
                }],
                distinct: false,
            },
        ]);
        let stripped_plan = PhysicalPlan {
            ops: full_plan.ops[2..].to_vec(),
            diagnostics: full_plan.diagnostics,
            annotations: full_plan.annotations,
            output: full_plan.output,
            binding_layout: full_plan.binding_layout,
        };
        let blob = encode_block_plans(&[stripped_plan], false).expect("encode stripped plan");
        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![
                SeedRowWire {
                    vertex_bindings: vec![SeedVertexBinding {
                        variable: "d".into(),
                        local_vertex_id: 9999,
                        required_vertex_label_ids: Vec::new(),
                    }],
                    float64_bindings: vec![SeedFloat64Binding {
                        variable: "distance".into(),
                        value: 0.0,
                    }],
                },
                SeedRowWire {
                    vertex_bindings: vec![SeedVertexBinding {
                        variable: "d".into(),
                        local_vertex_id: tombstoned_local,
                        required_vertex_label_ids: Vec::new(),
                    }],
                    float64_bindings: vec![SeedFloat64Binding {
                        variable: "distance".into(),
                        value: 1.0,
                    }],
                },
                SeedRowWire {
                    vertex_bindings: vec![SeedVertexBinding {
                        variable: "d".into(),
                        local_vertex_id: present_local,
                        required_vertex_label_ids: Vec::new(),
                    }],
                    float64_bindings: vec![SeedFloat64Binding {
                        variable: "distance".into(),
                        value: 2.0,
                    }],
                },
            ],
            complete_prefix_rows: false,
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
        .expect("wire row seed missing/tombstone skip");

        assert_eq!(run.row_count, 1);
    }

    #[test]
    fn wire_plan_unstripped_search_rejected_by_executor_contract() {
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql_planner::plan::{
            ProjectColumn, SearchOutputKind, SearchOutputPlan, SearchProviderPlan,
        };

        let store = GraphStore::new();
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: "d".into(),
                label: Some("Doc".into()),
                property_projection: None,
            },
            PlanOp::Search {
                binding: "d".into(),
                provider: SearchProviderPlan::VectorIndex {
                    index_name: vec!["doc_vec".into()],
                    query: Expr::var("query"),
                    limit: Expr::int(10),
                    filter: None,
                },
                output: SearchOutputPlan {
                    kind: SearchOutputKind::Distance,
                    alias: "distance".into(),
                },
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("d".into())),
                    alias: Some("d".into()),
                }],
                distinct: false,
            },
        ]);
        let blob = encode_block_plans(&[plan], false).expect("encode plan");
        let err = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &BTreeMap::new(),
            GqlCanisterExecutionMode::CompositeQuery,
            None,
            GqlExecutionContext::default(),
            None,
            None,
        ))
        .unwrap_err();
        assert!(
            err.to_string().contains(
                "SEARCH is parsed and planned but Router lowering is not implemented yet"
            ),
            "raw Search op must be rejected by executor contract: {err}"
        );
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
        let from_rows = PlanQueryResult::try_from_plan_rows(
            &store,
            &gleaph_graph_kernel::federation::ElementIdEncodingKey::host_test_fixture(),
            &binding_rows,
        )
        .expect("materialize");
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
        crate::test_labels::install_test_edge_inline_value_profile(
            label_id,
            gleaph_graph_kernel::entry::EdgeInlineValueProfile::from(EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            }),
        );
        store
            .insert_directed_edge_with_inline_value_bytes(a, b, Some(label_id), &1u16.to_le_bytes())
            .expect("a->b");
        store
            .insert_directed_edge_with_inline_value_bytes(b, c, Some(label_id), &1u16.to_le_bytes())
            .expect("b->c");
        store
            .insert_directed_edge_with_inline_value_bytes(
                a,
                c,
                Some(label_id),
                &100u16.to_le_bytes(),
            )
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

#[cfg(test)]
mod wave_4_regression_tests {
    use super::*;

    #[test]
    fn wave_4_regression_match_two_post_return_a_next_insert_reply_to() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use std::collections::BTreeMap;
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (:User {user_id: 'alice', demo_graph: 'social'})",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .unwrap();
        for demo_id in [284u64, 4284u64] {
            let mut p = BTreeMap::new();
            p.insert("$demo_id".to_string(), gleaph_gql::Value::Uint64(demo_id));
            p.insert(
                "$body".to_string(),
                gleaph_gql::Value::Text("x".to_string()),
            );
            p.insert("$is_public".to_string(), gleaph_gql::Value::Bool(true));
            pollster::block_on(run_adhoc_gql(
                store,
                "MATCH (a:User {user_id: 'alice', demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:POSTED {demo_edge_id: 'e', demo_kind: 'posted'}]->(b:Post {demo_id: $demo_id, demo_graph: 'social', body: $body, created_at: CURRENT_TIMESTAMP, is_public: $is_public})",
                &p,
                None,
                GqlCanisterExecutionMode::Update,
                GqlExecutionContext::default(),
            ))
            .unwrap();
        }
        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let result = pollster::block_on(execute_dml_plan_async(
            &store,
            &plan,
            &p,
            None,
            GqlExecutionContext::default(),
            None,
        ));
        result.expect("wave 4 shape must bind a and b");
    }

    #[test]
    fn wave_4_regression_wire_block_match_two_post_return_a_next_insert_reply_to() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use std::collections::BTreeMap;
        let store = GraphStore::new();
        let params = BTreeMap::new();
        pollster::block_on(run_adhoc_gql(
            store,
            "INSERT (:User {user_id: 'alice', demo_graph: 'social'})",
            &params,
            None,
            GqlCanisterExecutionMode::Update,
            GqlExecutionContext::default(),
        ))
        .unwrap();
        for demo_id in [284u64, 4284u64] {
            let mut p = BTreeMap::new();
            p.insert("$demo_id".to_string(), gleaph_gql::Value::Uint64(demo_id));
            p.insert(
                "$body".to_string(),
                gleaph_gql::Value::Text("x".to_string()),
            );
            p.insert("$is_public".to_string(), gleaph_gql::Value::Bool(true));
            pollster::block_on(run_adhoc_gql(
                store,
                "MATCH (a:User {user_id: 'alice', demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:POSTED {demo_edge_id: 'e', demo_kind: 'posted'}]->(b:Post {demo_id: $demo_id, demo_graph: 'social', body: $body, created_at: CURRENT_TIMESTAMP, is_public: $is_public})",
                &p,
                None,
                GqlCanisterExecutionMode::Update,
                GqlExecutionContext::default(),
            ))
            .unwrap();
        }
        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");
        let result = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            None,
            Some(1),
        ));
        result.expect("wire wave 4 shape must bind a and b");
    }

    #[test]
    fn wave_4_regression_complete_row_seed_skips_read_prefix() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};
        use std::collections::BTreeMap;

        let store = GraphStore::new();

        // Create the two endpoint posts directly so we know their local vertex ids.
        let a_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(4284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post a");
        let b_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post b");

        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");

        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![
                    SeedVertexBinding {
                        variable: "a".into(),
                        local_vertex_id: u32::try_from(u64::from(a_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                    SeedVertexBinding {
                        variable: "b".into(),
                        local_vertex_id: u32::try_from(u64::from(b_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                ],
                float64_bindings: Vec::new(),
            }],
            complete_prefix_rows: true,
        };

        let result = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            Some(1),
        ));
        result.expect("complete row seed must insert REPLY_TO edge");
    }
    #[test]
    fn wave_4_complete_row_seed_drops_stale_property_value() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};
        use std::collections::BTreeMap;

        let store = GraphStore::new();

        let a_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(4284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post a");
        let b_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post b");

        // Stale the canonical demo_id on `a` after the seed was produced.
        store
            .set_vertex_property(
                a_id,
                crate::test_labels::property_id_for_name("demo_id"),
                gleaph_gql::Value::Uint64(9999),
            )
            .expect("stale demo_id");

        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");

        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![
                    SeedVertexBinding {
                        variable: "a".into(),
                        local_vertex_id: u32::try_from(u64::from(a_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                    SeedVertexBinding {
                        variable: "b".into(),
                        local_vertex_id: u32::try_from(u64::from(b_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                ],
                float64_bindings: Vec::new(),
            }],
            complete_prefix_rows: true,
        };

        let result = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            Some(1),
        ))
        .expect("execution should complete");
        assert_eq!(
            result.row_count, 0,
            "stale property value must drop the row"
        );
    }

    #[test]
    fn wave_4_complete_row_seed_drops_missing_label() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};
        use std::collections::BTreeMap;

        let store = GraphStore::new();

        let a_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(4284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post a");
        let b_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post b");

        // Remove the Post label from `a` after the seed was produced.
        let a_vertex = store.vertex(a_id).expect("a exists");
        let post_label_id = store
            .vertex_labels(a_id, a_vertex)
            .into_iter()
            .find(|label| *label == crate::test_labels::vertex_label_id_for_name("Post"))
            .expect("Post label");
        store.remove_vertex_label(a_id, a_vertex, post_label_id);

        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");

        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![
                    SeedVertexBinding {
                        variable: "a".into(),
                        local_vertex_id: u32::try_from(u64::from(a_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                    SeedVertexBinding {
                        variable: "b".into(),
                        local_vertex_id: u32::try_from(u64::from(b_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                ],
                float64_bindings: Vec::new(),
            }],
            complete_prefix_rows: true,
        };

        let result = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            Some(1),
        ))
        .expect("execution should complete");
        assert_eq!(result.row_count, 0, "missing label must drop the row");
    }

    #[test]
    fn wave_4_complete_row_seed_drops_tombstoned_vertex() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};
        use std::collections::BTreeMap;

        let store = GraphStore::new();

        let a_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(4284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post a");
        let b_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post b");

        // Delete vertex a so it becomes a tombstone; seed hydration will already drop it, but
        // this test guards the end-to-end path.
        store.detach_delete_vertex(a_id).expect("delete a");

        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");

        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![
                    SeedVertexBinding {
                        variable: "a".into(),
                        local_vertex_id: u32::try_from(u64::from(a_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                    SeedVertexBinding {
                        variable: "b".into(),
                        local_vertex_id: u32::try_from(u64::from(b_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                ],
                float64_bindings: Vec::new(),
            }],
            complete_prefix_rows: true,
        };

        let result = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            Some(1),
        ))
        .expect("execution should complete");
        assert_eq!(result.row_count, 0, "tombstoned vertex must drop the row");
    }

    #[test]
    fn wave_4_complete_row_seed_intersection_filters_stale_arm() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};
        use std::collections::BTreeMap;

        let store = GraphStore::new();

        let a_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(4284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                    ("topic", gleaph_gql::Value::Text("ic".into())),
                ],
            )
            .expect("insert post a");
        let b_id = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(284)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert post b");

        // Stale the `topic` arm of an intersection while demo_id stays correct.
        store
            .set_vertex_property(
                a_id,
                crate::test_labels::property_id_for_name("topic"),
                gleaph_gql::Value::Text("other".into()),
            )
            .expect("stale topic");

        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(4284));
        p.insert("$a_topic".to_string(), gleaph_gql::Value::Text("ic".into()));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(284));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, topic: $a_topic, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");

        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![
                    SeedVertexBinding {
                        variable: "a".into(),
                        local_vertex_id: u32::try_from(u64::from(a_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                    SeedVertexBinding {
                        variable: "b".into(),
                        local_vertex_id: u32::try_from(u64::from(b_id)).unwrap(),
                        required_vertex_label_ids: Vec::new(),
                    },
                ],
                float64_bindings: Vec::new(),
            }],
            complete_prefix_rows: true,
        };

        let result = pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            Some(1),
        ))
        .expect("execution should complete");
        assert_eq!(
            result.row_count, 0,
            "stale intersection arm must drop the row"
        );
    }

    #[test]
    fn wave_4_complete_row_seed_keeps_matching_product_rows() {
        use gleaph_gql::{parser, type_check::NoSchema};
        use gleaph_gql_planner::build_block_plan_with_schema;
        use gleaph_gql_planner::wire::encode_block_plans;
        use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};
        use std::collections::BTreeMap;

        let store = GraphStore::new();

        // Two source posts and two target posts; only (a1,b1) matches the parameter filters.
        let a1 = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(1)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert a1");
        let a2 = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(2)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert a2");
        let b1 = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(3)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert b1");
        let b2 = store
            .insert_vertex_named(
                ["Post"],
                [
                    ("demo_id", gleaph_gql::Value::Uint64(4)),
                    ("demo_graph", gleaph_gql::Value::Text("social".into())),
                ],
            )
            .expect("insert b2");

        let mut p = BTreeMap::new();
        p.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(1));
        p.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(3));
        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).unwrap();
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();
        let plan = build_block_plan_with_schema(block, None, &NoSchema).unwrap();
        let blob = encode_block_plans(&[plan], true).expect("encode plan");

        let seeds = SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![
                SeedRowWire {
                    vertex_bindings: vec![
                        SeedVertexBinding {
                            variable: "a".into(),
                            local_vertex_id: u32::try_from(u64::from(a1)).unwrap(),
                            required_vertex_label_ids: Vec::new(),
                        },
                        SeedVertexBinding {
                            variable: "b".into(),
                            local_vertex_id: u32::try_from(u64::from(b1)).unwrap(),
                            required_vertex_label_ids: Vec::new(),
                        },
                    ],
                    float64_bindings: Vec::new(),
                },
                SeedRowWire {
                    vertex_bindings: vec![
                        SeedVertexBinding {
                            variable: "a".into(),
                            local_vertex_id: u32::try_from(u64::from(a2)).unwrap(),
                            required_vertex_label_ids: Vec::new(),
                        },
                        SeedVertexBinding {
                            variable: "b".into(),
                            local_vertex_id: u32::try_from(u64::from(b2)).unwrap(),
                            required_vertex_label_ids: Vec::new(),
                        },
                    ],
                    float64_bindings: Vec::new(),
                },
            ],
            complete_prefix_rows: true,
        };

        pollster::block_on(run_wire_plan_last_read_row_count(
            store,
            &blob,
            &p,
            GqlCanisterExecutionMode::Update,
            None,
            GqlExecutionContext::default(),
            Some(seeds),
            Some(1),
        ))
        .expect("execution should complete");
        let reply_edges = |vid| GraphStore::new().directed_out_edges(vid).unwrap().len();
        assert_eq!(
            reply_edges(a1) + reply_edges(a2),
            1,
            "only the supplied row matching the read-prefix filters must mutate"
        );
    }
}
