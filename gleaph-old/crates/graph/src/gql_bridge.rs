use gleaph_gql::{
    ast::{CreateStmt, Expr, MergeStmt, QueryStmt, SetItem, ShowTarget, Statement},
    executor::{
        ExecutionLimits, MutationOutcome, MutationProgress, clear_binary_pad_defs, clear_caller,
        clear_char_pad_defs, clear_current_time, clear_node_type_defs, execute_mutation_resumable,
        execute_mutation_tracked, execute_plan_with_limits_and_hasher,
        execute_plan_with_params_and_hasher, execute_query_statement_with_limits,
        set_binary_pad_defs, set_caller, set_char_pad_defs, set_current_time, set_node_type_defs,
    },
    planner::{build_plan_with_stats, build_runtime_plan_with_stats, explain_plan_with_stats},
    semantic::analyze_statement_structure,
    stats::TableStats,
};
use gleaph_gql::{parse_statement, parse_statement_from_tokens, validate_statement};
use gleaph_gql::lexer::tokenize;
use gleaph_pma::RapidRandomState;
use gleaph_types::{EntityType, GleaphError, IndexType, MutationResult, QueryResult, Value};

use crate::state::{with_state, with_state_mut};

/// Returns the caller principal as a `Value::Principal`.
fn caller_value() -> Value {
    #[cfg(target_arch = "wasm32")]
    {
        Value::Principal(ic_cdk::api::msg_caller())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Value::Principal(candid::Principal::anonymous())
    }
}

/// Injects the caller principal into the executor thread-local and returns a
/// drop-guard that clears it.
fn inject_caller() -> impl Drop {
    set_caller(caller_value());
    struct ClearCallerGuard;
    impl Drop for ClearCallerGuard {
        fn drop(&mut self) {
            clear_caller();
        }
    }
    ClearCallerGuard
}

// ── Schema-aware type inference (§18.9): Schema-aware type inference bridge ─────────────────────

/// Implements `PropertySchema` by reading the active graph type's stored node/edge types.
struct ActiveGraphTypeSchema {
    node_types: Vec<crate::state::StoredNodeType>,
    edge_types: Vec<crate::state::StoredEdgeType>,
}

impl ActiveGraphTypeSchema {
    fn from_active() -> Self {
        match crate::state::get_active_graph_type() {
            Some(gt) => Self {
                node_types: gt.node_types,
                edge_types: gt.edge_types,
            },
            None => Self {
                node_types: vec![],
                edge_types: vec![],
            },
        }
    }
}

fn stored_scalar_to_ast(s: crate::state::StoredScalarType) -> gleaph_gql::ast::ScalarType {
    use crate::state::StoredScalarType as S;
    use gleaph_gql::ast::ScalarType as A;
    match s {
        S::Int64 => A::Int64,
        S::Float64 => A::Float64,
        S::Float32 => A::Float32,
        S::Text => A::Text,
        S::Bool => A::Bool,
        S::Timestamp => A::Timestamp,
        S::Bytes => A::Bytes,
        S::Date => A::Date,
        S::Time => A::Time,
        S::DateTime => A::DateTime,
        S::Duration => A::Duration,
        S::Principal => A::Principal,
        S::Decimal => A::Decimal,
        S::Uint64 => A::Uint64,
        S::Int8 => A::Int8,
        S::Int16 => A::Int16,
        S::Int32 => A::Int32,
        S::Int128 => A::Int128,
        S::Int256 => A::Int256,
        S::Uint8 => A::Uint8,
        S::Uint16 => A::Uint16,
        S::Uint32 => A::Uint32,
        S::Uint128 => A::Uint128,
        S::Uint256 => A::Uint256,
    }
}

fn stored_to_value_type(svt: crate::state::StoredValueType) -> gleaph_gql::ast::ValueType {
    use crate::state::StoredValueType as S;
    use gleaph_gql::ast::ValueType as V;
    match svt {
        S::Int64 => V::Int64,
        S::Float64 => V::Float64,
        S::Float32 => V::Float32,
        S::Text => V::Text,
        S::Bool => V::Bool,
        S::Timestamp => V::Timestamp,
        S::List => V::List,
        S::TypedList(s) => V::TypedList(stored_scalar_to_ast(s)),
        S::Bytes => V::Bytes,
        S::Date => V::Date,
        S::Time => V::Time,
        S::DateTime => V::DateTime,
        S::Duration => V::Duration,
        S::Decimal => V::Decimal,
        S::Uint64 => V::Uint64,
        S::Int8 => V::Int8,
        S::Int16 => V::Int16,
        S::Int32 => V::Int32,
        S::Int128 => V::Int128,
        S::Int256 => V::Int256,
        S::Uint8 => V::Uint8,
        S::Uint16 => V::Uint16,
        S::Uint32 => V::Uint32,
        S::Uint128 => V::Uint128,
        S::Uint256 => V::Uint256,
        S::TextConstrained {
            min_length,
            max_length,
            fixed,
        } => V::TextConstrained {
            min_length,
            max_length,
            fixed,
        },
        S::BytesConstrained {
            min_length,
            max_length,
            fixed,
        } => V::BytesConstrained {
            min_length,
            max_length,
            fixed,
        },
    }
}

impl gleaph_gql::type_check::PropertySchema for ActiveGraphTypeSchema {
    fn node_property_types(
        &self,
        labels: &[String],
    ) -> Vec<(String, gleaph_gql::ast::ValueType, bool)> {
        // Find node types whose labels are a subset of the given labels.
        let mut result = Vec::new();
        for nt in &self.node_types {
            if nt.labels.iter().all(|l| labels.contains(l)) {
                for pd in &nt.properties {
                    result.push((
                        pd.name.clone(),
                        stored_to_value_type(pd.value_type),
                        pd.required,
                    ));
                }
            }
        }
        result
    }

    fn edge_property_types(&self, label: &str) -> Vec<(String, gleaph_gql::ast::ValueType, bool)> {
        let mut result = Vec::new();
        for et in &self.edge_types {
            if et.label.eq_ignore_ascii_case(label) {
                for pd in &et.properties {
                    result.push((
                        pd.name.clone(),
                        stored_to_value_type(pd.value_type),
                        pd.required,
                    ));
                }
            }
        }
        result
    }

    fn resolve_node_type_labels(&self, type_name: &str) -> Option<Vec<String>> {
        self.node_types
            .iter()
            .find(|nt| nt.name.eq_ignore_ascii_case(type_name))
            .map(|nt| nt.labels.clone())
    }

    fn edge_endpoint_types(&self, label: &str) -> Vec<(Vec<String>, Vec<String>)> {
        self.edge_types
            .iter()
            .filter(|et| et.label.eq_ignore_ascii_case(label))
            .map(|et| {
                let from = et
                    .from_types
                    .iter()
                    .flat_map(|type_name| {
                        self.resolve_node_type_labels(type_name)
                            .unwrap_or_else(|| vec![type_name.clone()])
                    })
                    .collect::<Vec<_>>();
                let to = et
                    .to_types
                    .iter()
                    .flat_map(|type_name| {
                        self.resolve_node_type_labels(type_name)
                            .unwrap_or_else(|| vec![type_name.clone()])
                    })
                    .collect::<Vec<_>>();
                (from, to)
            })
            .collect()
    }

    fn resolve_edge_type(&self, type_name: &str) -> Option<(String, Vec<String>, Vec<String>)> {
        self.edge_types
            .iter()
            .find(|et| et.name.eq_ignore_ascii_case(type_name))
            .map(|et| {
                let from = et
                    .from_types
                    .iter()
                    .flat_map(|name| {
                        self.resolve_node_type_labels(name)
                            .unwrap_or_else(|| vec![name.clone()])
                    })
                    .collect::<Vec<_>>();
                let to = et
                    .to_types
                    .iter()
                    .flat_map(|name| {
                        self.resolve_node_type_labels(name)
                            .unwrap_or_else(|| vec![name.clone()])
                    })
                    .collect::<Vec<_>>();
                (et.label.clone(), from, to)
            })
    }
}

/// Pushes active graph type node type definitions into the executor thread-local.
fn sync_node_type_defs() {
    if let Some(gt) = crate::state::get_active_graph_type() {
        let mut defs = std::collections::HashMap::new();
        for nt in &gt.node_types {
            defs.insert(nt.name.to_ascii_lowercase(), nt.labels.clone());
            defs.insert(nt.name.clone(), nt.labels.clone());
        }
        set_node_type_defs(defs);
        sync_char_pad_defs(&gt);
    } else {
        clear_node_type_defs();
        clear_char_pad_defs();
        clear_binary_pad_defs();
    }
}

/// Collects CHAR(n) and BINARY(n) property constraints from the active graph type
/// and injects them into the executor's thread-locals for read-time padding.
fn sync_char_pad_defs(gt: &crate::state::StoredGraphType) {
    use crate::state::StoredValueType;
    let mut char_defs = std::collections::HashMap::new();
    let mut binary_defs = std::collections::HashMap::new();
    let all_props = gt
        .node_types
        .iter()
        .flat_map(|nt| &nt.properties)
        .chain(gt.edge_types.iter().flat_map(|et| &et.properties));
    for pd in all_props {
        match pd.value_type {
            StoredValueType::TextConstrained {
                max_length,
                fixed: true,
                ..
            } => {
                char_defs.insert(pd.name.clone(), max_length);
            }
            StoredValueType::BytesConstrained {
                max_length,
                fixed: true,
                ..
            } => {
                binary_defs.insert(pd.name.clone(), max_length);
            }
            _ => {}
        }
    }
    if char_defs.is_empty() {
        clear_char_pad_defs();
    } else {
        set_char_pad_defs(char_defs);
    }
    if binary_defs.is_empty() {
        clear_binary_pad_defs();
    } else {
        set_binary_pad_defs(binary_defs);
    }
}

/// Returns the current IC time in nanoseconds (0 in non-wasm tests).
fn ic_timestamp() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

const MAX_QUERY_LEN_HARD: usize = 16 * 1024;
const DEFAULT_MAX_ROWS: usize = 1_000;
const HARD_MAX_ROWS: usize = 10_000;
const HARD_MAX_EXECUTION_STEPS: u64 = 1_000_000;
const HARD_MAX_EXECUTION_STEPS_HEAVY_MUTATION: u64 = 10_000_000_000;
const HARD_MAX_EXECUTION_STEPS_HEAVY_AGG_QUERY: u64 = 10_000_000_000;

/// IC query call instruction limit (5 billion).
#[allow(dead_code)]
pub const IC_QUERY_INSTRUCTION_LIMIT: u64 = 5_000_000_000;
/// IC update call instruction limit (40 billion).
#[allow(dead_code)]
pub const IC_UPDATE_INSTRUCTION_LIMIT: u64 = 40_000_000_000;

#[cfg(test)]
thread_local! {
    static TEST_FAIL_AFTER_MUTATION_ONCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Capped query used by benchmarks. Auto-fallback in `api::query_gql` supersedes this for
/// the IC endpoint, but benchmarks still call this directly.
#[allow(dead_code)]
pub fn query(gql: &str) -> Result<QueryResult, GleaphError> {
    let result = query_internal(gql, DEFAULT_MAX_ROWS, None)?;
    enforce_result_limits(&result)?;
    Ok(result)
}

/// Like `query`, but allows up to `HARD_MAX_ROWS` results for cursor-based pagination.
pub fn query_paged(gql: &str) -> Result<QueryResult, GleaphError> {
    query_internal(gql, HARD_MAX_ROWS, None)
}

pub fn query_paged_with_params(
    gql: &str,
    params: &std::collections::HashMap<String, Value>,
) -> Result<QueryResult, GleaphError> {
    query_internal(gql, HARD_MAX_ROWS, Some(params))
}

/// Returns a stable planner explanation for a validated query as a one-column table.
pub fn explain(gql: &str) -> Result<QueryResult, GleaphError> {
    enforce_limits(gql)?;
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;

    if matches!(stmt, Statement::Query(_)) {
        let stats = current_table_stats()?;
        let mut lines = explain_plan_with_stats(&stmt, Some(&stats))?;
        let schema = ActiveGraphTypeSchema::from_active();
        let type_warnings =
            gleaph_gql::type_check::type_check_statement_with_schema(&stmt, &schema);
        lines.extend(explain_type_warning_summary_lines(&type_warnings));
        if !type_warnings.is_empty() {
            lines.extend(type_warnings.iter().map(|warning| {
                format!(
                    "type-warning={}:{}",
                    explain_warning_kind(warning.kind),
                    warning.message
                )
            }));
        }
        return Ok(QueryResult {
            columns: vec!["line".into()],
            rows: lines
                .into_iter()
                .map(|line| vec![Value::Text(line)])
                .collect(),
            stats: gleaph_types::QueryStats::default(),
            warnings: structured_type_diagnostics(type_warnings),
        });
    }

    Err(GleaphError::ExecutionError(
        "EXPLAIN currently supports query statements only".into(),
    ))
}

fn explain_warning_kind(kind: gleaph_gql::type_check::WarningKind) -> &'static str {
    use gleaph_gql::type_check::WarningKind;
    match kind {
        WarningKind::BinaryOpMismatch => "BinaryOpMismatch",
        WarningKind::NonBooleanCondition => "NonBooleanCondition",
        WarningKind::FunctionArgMismatch => "FunctionArgMismatch",
        WarningKind::ComparisonMismatch => "ComparisonMismatch",
        WarningKind::NullCheckOnNonNull => "NullCheckOnNonNull",
        WarningKind::ImpossiblePattern => "ImpossiblePattern",
        WarningKind::GroupingViolation => "GroupingViolation",
    }
}

fn prepared_warning_kind(
    kind: gleaph_gql::type_check::WarningKind,
) -> gleaph_types::TypeDiagnosticKind {
    use gleaph_gql::type_check::WarningKind;
    match kind {
        WarningKind::BinaryOpMismatch => gleaph_types::TypeDiagnosticKind::BinaryOpMismatch,
        WarningKind::NonBooleanCondition => gleaph_types::TypeDiagnosticKind::NonBooleanCondition,
        WarningKind::FunctionArgMismatch => gleaph_types::TypeDiagnosticKind::FunctionArgMismatch,
        WarningKind::ComparisonMismatch => gleaph_types::TypeDiagnosticKind::ComparisonMismatch,
        WarningKind::NullCheckOnNonNull => gleaph_types::TypeDiagnosticKind::NullCheckOnNonNull,
        WarningKind::ImpossiblePattern => gleaph_types::TypeDiagnosticKind::ImpossiblePattern,
        WarningKind::GroupingViolation => gleaph_types::TypeDiagnosticKind::GroupingViolation,
    }
}

fn info_diagnostic(message: impl Into<String>) -> gleaph_types::TypeDiagnostic {
    gleaph_types::TypeDiagnostic {
        kind: gleaph_types::TypeDiagnosticKind::Info,
        message: message.into(),
    }
}

fn structured_type_diagnostics(
    warnings: Vec<gleaph_gql::type_check::TypeWarning>,
) -> Vec<gleaph_types::TypeDiagnostic> {
    warnings
        .into_iter()
        .map(|warning| gleaph_types::TypeDiagnostic {
            kind: prepared_warning_kind(warning.kind),
            message: warning.message,
        })
        .collect()
}

fn has_impossible_pattern_warning(warnings: &[gleaph_gql::type_check::TypeWarning]) -> bool {
    warnings
        .iter()
        .any(|warning| warning.kind == gleaph_gql::type_check::WarningKind::ImpossiblePattern)
}

fn explain_type_warning_summary_lines(
    warnings: &[gleaph_gql::type_check::TypeWarning],
) -> Vec<String> {
    let impossible_count = warnings
        .iter()
        .filter(|warning| warning.kind == gleaph_gql::type_check::WarningKind::ImpossiblePattern)
        .count();
    let mut lines = vec![format!("type-warning-count={}", warnings.len())];
    if impossible_count > 0 {
        lines.push("semantic-impossible-pattern=true".into());
        lines.push(format!(
            "semantic-impossible-pattern-count={impossible_count}"
        ));
    }
    lines
}

fn impossible_query_result(
    stmt: &Statement,
    warnings: Vec<gleaph_types::TypeDiagnostic>,
) -> QueryResult {
    QueryResult {
        columns: extract_columns(stmt),
        rows: vec![],
        stats: gleaph_types::QueryStats::default(),
        warnings,
    }
}

/// Executes a read query with caller-specified execution limits.
///
/// Intended for benchmark diagnostics only.
#[allow(dead_code)]
pub fn query_with_limits(
    gql: &str,
    max_rows: Option<usize>,
    max_execution_steps: Option<u64>,
) -> Result<QueryResult, GleaphError> {
    // Keep cheap input validation ahead of `with_state(...)` so malformed/oversized requests
    // return deterministic parser/guardrail errors even when state is not initialized in tests.
    enforce_limits(gql)?;
    #[cfg(feature = "canbench-rs")]
    let _scope_parse = canbench_rs::bench_scope("parse");
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;
    #[cfg(feature = "canbench-rs")]
    drop(_scope_parse);

    // §16.2 USE GRAPH: USE GRAPH resolves alias and returns canister_id as informational result.
    if let Statement::UseGraph(ref name) = stmt {
        return resolve_use_graph(name);
    }

    // §12: DESCRIBE GRAPH TYPE — introspect a graph type schema.
    if let Statement::DescribeGraphType(ref name) = stmt {
        return resolve_describe_graph_type(name);
    }

    // SHOW ... — read-only introspection statements.
    if let Statement::Show(ref target) = stmt {
        return resolve_show(target);
    }

    // CALL procedure(...) YIELD ... — built-in algorithm invocation.
    if let Statement::CallProcedure(ref call) = stmt {
        return execute_call_procedure(call);
    }

    // CREATE/DROP GRAPH require dedicated async endpoints.
    if matches!(
        stmt,
        Statement::CreateGraph { .. } | Statement::DropGraph { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "graph catalog operations require the execute_gql endpoint".into(),
        ));
    }

    // Other mutation-only statements: reject in query context.
    if is_mutation_only_statement(&stmt) {
        return Err(GleaphError::ExecutionError(
            "this statement requires the mutation endpoint".into(),
        ));
    }

    // §12 Type annotations: Sync node type definitions for type annotation resolution.
    sync_node_type_defs();
    // Inject IC time and caller principal into executor thread-locals.
    set_current_time(ic_timestamp());
    let _caller_guard = inject_caller();
    struct ClearTimeGuardLimits;
    impl Drop for ClearTimeGuardLimits {
        fn drop(&mut self) {
            clear_current_time();
        }
    }
    let _clear_time = ClearTimeGuardLimits;

    let schema = ActiveGraphTypeSchema::from_active();
    let type_warnings = gleaph_gql::type_check::type_check_statement_with_schema(&stmt, &schema);
    if matches!(stmt, Statement::Query(_)) && has_impossible_pattern_warning(&type_warnings) {
        return Ok(impossible_query_result(
            &stmt,
            structured_type_diagnostics(type_warnings),
        ));
    }

    #[cfg(feature = "canbench-rs")]
    let _scope_plan = canbench_rs::bench_scope("plan");
    let default_steps = if statement_has_aggregation(&stmt) {
        HARD_MAX_EXECUTION_STEPS_HEAVY_AGG_QUERY
    } else {
        HARD_MAX_EXECUTION_STEPS
    };
    let query_max_execution_steps = max_execution_steps.unwrap_or(default_steps);

    // Compound statements (UNION/EXCEPT/INTERSECT) bypass the physical planner and
    // execute each branch directly, applying set semantics in memory.
    if matches!(stmt, Statement::Compound { .. }) {
        let result = with_state(|g| {
            execute_query_statement_with_limits(
                &stmt,
                g,
                ExecutionLimits {
                    max_rows,
                    max_execution_steps: Some(query_max_execution_steps),
                },
            )
        })?;
        return Ok(result);
    }

    let stats = current_table_stats()?;
    let mut plan = build_runtime_plan_with_stats(&stmt, Some(&stats))?;
    // Attach type diagnostics to the plan so they're available via explain.
    if !type_warnings.is_empty() {
        plan.annotations.type_diagnostics = Some(type_warnings);
    }
    #[cfg(feature = "canbench-rs")]
    drop(_scope_plan);

    #[cfg(feature = "canbench-rs")]
    let _scope_exec = canbench_rs::bench_scope("execute");
    let hasher = RapidRandomState::new();
    let result = with_state(|g| {
        execute_plan_with_limits_and_hasher(
            &plan,
            g,
            ExecutionLimits {
                max_rows,
                max_execution_steps: Some(query_max_execution_steps),
            },
            &hasher,
        )
    })?;
    Ok(result)
}

/// Like [`query_with_limits`] but also returns per-stage instruction deltas.
///
/// Intended for benchmark diagnostics to distinguish planner/bridge overhead from
/// executor work reported in `QueryResult.stats`.
#[allow(dead_code)]
pub fn query_with_limits_profiled(
    gql: &str,
    max_rows: Option<usize>,
    max_execution_steps: Option<u64>,
) -> Result<(QueryResult, Vec<(String, u64)>), GleaphError> {
    let mut stages: Vec<(String, u64)> = Vec::new();
    let mut stage_start = ic_cdk::api::performance_counter(0);
    let mut push_stage = |name: &str, stage_start: &mut u64| {
        let now = ic_cdk::api::performance_counter(0);
        stages.push((name.to_string(), now.saturating_sub(*stage_start)));
        *stage_start = now;
    };

    enforce_limits(gql)?;
    push_stage("enforce_limits", &mut stage_start);

    let tokens = tokenize(gql)?;
    push_stage("tokenize", &mut stage_start);

    let stmt = parse_statement_from_tokens(&tokens)?;
    push_stage("parse_statement", &mut stage_start);

    validate_statement(&stmt)?;
    push_stage("validate_statement", &mut stage_start);

    if let Statement::UseGraph(ref name) = stmt {
        let result = resolve_use_graph(name)?;
        push_stage("resolve_use_graph", &mut stage_start);
        return Ok((result, stages));
    }

    if let Statement::DescribeGraphType(ref name) = stmt {
        let result = resolve_describe_graph_type(name)?;
        push_stage("resolve_describe_graph_type", &mut stage_start);
        return Ok((result, stages));
    }

    if matches!(
        stmt,
        Statement::CreateGraph { .. } | Statement::DropGraph { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "graph catalog operations require the execute_gql endpoint".into(),
        ));
    }

    if matches!(
        stmt,
        Statement::CreateGraphType { .. } | Statement::DropGraphType { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "graph type operations require the mutation endpoint".into(),
        ));
    }

    if matches!(
        stmt,
        Statement::CreateSchema { .. } | Statement::DropSchema { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "schema operations require the mutation endpoint".into(),
        ));
    }

    sync_node_type_defs();
    push_stage("sync_node_type_defs", &mut stage_start);

    let default_steps = if statement_has_aggregation(&stmt) {
        HARD_MAX_EXECUTION_STEPS_HEAVY_AGG_QUERY
    } else {
        HARD_MAX_EXECUTION_STEPS
    };
    let query_max_execution_steps = max_execution_steps.unwrap_or(default_steps);
    push_stage("derive_execution_limits", &mut stage_start);

    if matches!(stmt, Statement::Compound { .. }) {
        let result = with_state(|g| {
            execute_query_statement_with_limits(
                &stmt,
                g,
                ExecutionLimits {
                    max_rows,
                    max_execution_steps: Some(query_max_execution_steps),
                },
            )
        })?;
        push_stage("execute_compound", &mut stage_start);
        return Ok((result, stages));
    }

    let stats = current_table_stats()?;
    push_stage("collect_table_stats", &mut stage_start);

    let _semantic = analyze_statement_structure(&stmt);
    push_stage("analyze_statement_structure", &mut stage_start);

    let plan = build_runtime_plan_with_stats(&stmt, Some(&stats))?;
    push_stage("build_plan_with_stats", &mut stage_start);

    let hasher = RapidRandomState::new();
    push_stage("build_hasher", &mut stage_start);

    let result = with_state(|g| {
        execute_plan_with_limits_and_hasher(
            &plan,
            g,
            ExecutionLimits {
                max_rows,
                max_execution_steps: Some(query_max_execution_steps),
            },
            &hasher,
        )
    })?;
    push_stage("execute_plan_with_limits_and_hasher", &mut stage_start);

    Ok((result, stages))
}

fn query_internal(
    gql: &str,
    max_rows: usize,
    params: Option<&std::collections::HashMap<String, Value>>,
) -> Result<QueryResult, GleaphError> {
    // Keep cheap input validation ahead of `with_state(...)` so malformed/oversized requests
    // return deterministic parser/guardrail errors even when state is not initialized in tests.
    enforce_limits(gql)?;
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;

    // §16.2 USE GRAPH: USE GRAPH resolves alias and returns canister_id as informational result.
    if let Statement::UseGraph(ref name) = stmt {
        return resolve_use_graph(name);
    }

    // §12: DESCRIBE GRAPH TYPE — introspect a graph type schema.
    if let Statement::DescribeGraphType(ref name) = stmt {
        return resolve_describe_graph_type(name);
    }

    // SHOW ... — read-only introspection statements.
    if let Statement::Show(ref target) = stmt {
        return resolve_show(target);
    }

    // CALL procedure(...) YIELD ... — built-in algorithm invocation.
    if let Statement::CallProcedure(ref call) = stmt {
        return execute_call_procedure(call);
    }

    // CREATE/DROP GRAPH require dedicated async endpoints.
    if matches!(
        stmt,
        Statement::CreateGraph { .. } | Statement::DropGraph { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "graph catalog operations require the execute_gql endpoint".into(),
        ));
    }

    // Other mutation-only statements: reject in query context.
    if is_mutation_only_statement(&stmt) {
        return Err(GleaphError::ExecutionError(
            "this statement requires the mutation endpoint".into(),
        ));
    }

    // §18.9: Static type inference — schema-aware. Phase 3: strict mode rejects mismatches.
    let schema = ActiveGraphTypeSchema::from_active();
    if crate::state::is_strict_type_check() {
        gleaph_gql::type_check::type_check_statement_strict(&stmt, &schema)?;
    }
    let type_warnings = gleaph_gql::type_check::type_check_statement_with_schema(&stmt, &schema);
    if matches!(stmt, Statement::Query(_)) && has_impossible_pattern_warning(&type_warnings) {
        return Ok(impossible_query_result(
            &stmt,
            structured_type_diagnostics(type_warnings),
        ));
    }
    let type_warnings = structured_type_diagnostics(type_warnings);

    // §12 Type annotations: Sync node type definitions for type annotation resolution.
    sync_node_type_defs();

    // Inject IC time and caller principal into executor thread-locals.
    set_current_time(ic_timestamp());
    let _caller_guard = inject_caller();
    struct ClearTimeGuard;
    impl Drop for ClearTimeGuard {
        fn drop(&mut self) {
            clear_current_time();
        }
    }
    let _clear_time = ClearTimeGuard;

    // Compound statements (UNION/EXCEPT/INTERSECT) bypass the physical planner and
    // execute each branch directly, applying set semantics in memory.
    if matches!(stmt, Statement::Compound { .. }) {
        let query_max_execution_steps = if statement_has_aggregation(&stmt) {
            HARD_MAX_EXECUTION_STEPS_HEAVY_AGG_QUERY
        } else {
            HARD_MAX_EXECUTION_STEPS
        };
        let mut result = with_state(|g| {
            execute_query_statement_with_limits(
                &stmt,
                g,
                ExecutionLimits {
                    max_rows: Some(max_rows),
                    max_execution_steps: Some(query_max_execution_steps),
                },
            )
        })?;
        result.warnings = type_warnings;
        return Ok(result);
    }

    let plan = with_state(|g| -> Result<_, GleaphError> {
        let vertex_count = g.vertex_count();
        let edge_count = g.edge_count();
        let mut stats = TableStats {
            vertex_count,
            edge_count,
            avg_degree: if vertex_count == 0 {
                1.0
            } else {
                (edge_count as f64 / vertex_count as f64).max(1.0)
            },
            label_cardinality: g.label_cardinalities(),
            ..TableStats::default()
        };
        // Use measured selectivity from equality index; fall back to 0.1 for indexed props
        // that haven't been measured yet.
        for (key, &sel) in g.get_property_selectivity() {
            stats.property_selectivity.insert(key.clone(), sel);
        }
        for idx in g.list_property_indexes() {
            if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
                stats
                    .indexed_vertex_properties
                    .insert(idx.property_name.clone());
                stats
                    .property_selectivity
                    .entry(format!("vertex:{}", idx.property_name))
                    .or_insert(0.1);
            }
            if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Range {
                stats
                    .range_indexed_vertex_properties
                    .insert(idx.property_name.clone());
            }
            if idx.entity_type == EntityType::Edge && idx.index_type == IndexType::Equality {
                stats
                    .indexed_edge_properties
                    .insert(idx.property_name.clone());
                stats
                    .property_selectivity
                    .entry(format!("edge:{}", idx.property_name))
                    .or_insert(0.1);
            }
        }
        build_runtime_plan_with_stats(&stmt, Some(&stats))
    })?;
    let query_max_execution_steps = if statement_has_aggregation(&stmt) {
        HARD_MAX_EXECUTION_STEPS_HEAVY_AGG_QUERY
    } else {
        HARD_MAX_EXECUTION_STEPS
    };
    let hasher = RapidRandomState::new();
    let limits = ExecutionLimits {
        max_rows: Some(max_rows),
        max_execution_steps: Some(query_max_execution_steps),
    };
    let result = with_state(|g| {
        if let Some(p) = params {
            execute_plan_with_params_and_hasher(&plan, g, p, limits, &hasher)
        } else {
            execute_plan_with_limits_and_hasher(&plan, g, limits, &hasher)
        }
    })?;
    let mut result = result;
    result.warnings = type_warnings;
    Ok(result)
}

fn statement_has_aggregation(stmt: &Statement) -> bool {
    match stmt {
        Statement::Query(q) => query_has_aggregation(q),
        Statement::Compound { left, right, .. } => {
            statement_has_aggregation(left) || statement_has_aggregation(right)
        }
        _ => false,
    }
}

fn query_has_aggregation(q: &QueryStmt) -> bool {
    q.return_clause
        .items
        .iter()
        .any(|i| expr_has_aggregation(&i.expr))
        || q.order_by
            .as_ref()
            .is_some_and(|o| o.items.iter().any(|i| expr_has_aggregation(&i.expr)))
        || q.group_by.is_some()
        || q.having.as_ref().is_some_and(expr_has_aggregation)
}

fn expr_has_aggregation(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate(_) => true,
        Expr::PropertyAccess { target, .. }
        | Expr::UnaryOp { expr: target, .. }
        | Expr::Not(target)
        | Expr::IsNull(target)
        | Expr::IsNotNull(target)
        | Expr::PathLength(target) => expr_has_aggregation(target),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::ListIndex {
            list: left,
            index: right,
        }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => expr_has_aggregation(left) || expr_has_aggregation(right),
        Expr::InList { expr, list, .. } => {
            expr_has_aggregation(expr) || list.iter().any(expr_has_aggregation)
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            expr_has_aggregation(expr) || expr_has_aggregation(pattern)
        }
        Expr::Case(c) => {
            c.operand.as_ref().is_some_and(|e| expr_has_aggregation(e))
                || c.when_then
                    .iter()
                    .any(|wt| expr_has_aggregation(&wt.when) || expr_has_aggregation(&wt.then))
                || c.else_expr
                    .as_ref()
                    .is_some_and(|e| expr_has_aggregation(e))
        }
        Expr::Coalesce(items) | Expr::ListLiteral(items) => items.iter().any(expr_has_aggregation),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_has_aggregation),
        Expr::Exists(_)
        | Expr::Literal(_)
        | Expr::Variable(_)
        | Expr::PathVar(_)
        | Expr::Parameter { .. } => false,
        Expr::Cast { expr, .. }
        | Expr::IsTruth { expr, .. }
        | Expr::IsLabeled { expr, .. }
        | Expr::IsDirected { expr, .. } => expr_has_aggregation(expr),
        Expr::IsSourceOf { node, edge, .. } | Expr::IsDestOf { node, edge, .. } => {
            expr_has_aggregation(node) || expr_has_aggregation(edge)
        }
        Expr::AllDifferent(exprs) | Expr::Same(exprs) => exprs.iter().any(expr_has_aggregation),
        Expr::PropertyExists { target, .. } => expr_has_aggregation(target),
        Expr::RecordLiteral(pairs) => pairs.iter().any(|(_, e)| expr_has_aggregation(e)),
        Expr::IsType { expr, .. } => expr_has_aggregation(expr),
        Expr::ValueSubquery(_) => false,
        Expr::LetIn { bindings, body } => {
            bindings.iter().any(|(_, e)| expr_has_aggregation(e)) || expr_has_aggregation(body)
        }
        Expr::PathConstructor(elems) => elems.iter().any(expr_has_aggregation),
    }
}

#[allow(dead_code)]
pub fn mutate(gql: &str) -> Result<MutationResult, GleaphError> {
    mutate_tracked(gql).map(|o| o.result)
}

/// Resolves a graph alias and returns the canister_id as a single-row QueryResult.
pub fn resolve_use_graph_public(name: &str) -> Result<QueryResult, GleaphError> {
    resolve_use_graph(name)
}

fn resolve_use_graph(name: &str) -> Result<QueryResult, GleaphError> {
    match crate::state::resolve_graph_alias(name) {
        Some(principal) => Ok(QueryResult {
            columns: vec!["graph_name".into(), "canister_id".into()],
            rows: vec![vec![
                gleaph_types::Value::Text(name.to_string()),
                gleaph_types::Value::Text(principal.to_text()),
            ]],
            stats: gleaph_types::QueryStats::default(),
            warnings: vec![],
        }),
        None => Err(GleaphError::ExecutionError(format!(
            "unknown graph alias: '{name}'"
        ))),
    }
}

/// §12: Resolves a DESCRIBE GRAPH TYPE query and returns the schema as a multi-row QueryResult.
fn resolve_describe_graph_type(name: &str) -> Result<QueryResult, GleaphError> {
    let gt = crate::state::get_graph_type(name).ok_or_else(|| {
        GleaphError::ExecutionError(format!("graph type '{name}' does not exist"))
    })?;

    let columns = vec![
        "kind".into(),
        "name".into(),
        "label".into(),
        "labels".into(),
        "from_types".into(),
        "to_types".into(),
        "properties".into(),
    ];

    let null = Value::Null;
    let mut rows = Vec::new();

    // Node labels
    for label in &gt.node_labels {
        rows.push(vec![
            Value::Text("node".into()),
            null.clone(),
            Value::Text(label.clone()),
            null.clone(),
            null.clone(),
            null.clone(),
            null.clone(),
        ]);
    }

    // Edge labels
    for label in &gt.edge_labels {
        rows.push(vec![
            Value::Text("edge".into()),
            null.clone(),
            Value::Text(label.clone()),
            null.clone(),
            null.clone(),
            null.clone(),
            null.clone(),
        ]);
    }

    // Node types
    for nt in &gt.node_types {
        rows.push(vec![
            Value::Text("node_type".into()),
            Value::Text(nt.name.clone()),
            null.clone(),
            Value::List(nt.labels.iter().map(|l| Value::Text(l.clone())).collect()),
            null.clone(),
            null.clone(),
            format_property_defs(&nt.properties),
        ]);
    }

    // Edge types
    for et in &gt.edge_types {
        rows.push(vec![
            Value::Text("edge_type".into()),
            Value::Text(et.name.clone()),
            Value::Text(et.label.clone()),
            null.clone(),
            Value::List(
                et.from_types
                    .iter()
                    .map(|l| Value::Text(l.clone()))
                    .collect(),
            ),
            Value::List(et.to_types.iter().map(|l| Value::Text(l.clone())).collect()),
            format_property_defs(&et.properties),
        ]);
    }

    Ok(QueryResult {
        columns,
        rows,
        stats: gleaph_types::QueryStats::default(),
        warnings: vec![],
    })
}

/// Formats a list of stored property definitions as a `Value::List` of `"name::TYPE [NOT NULL]"` strings.
fn format_property_defs(props: &[crate::state::StoredPropertyDef]) -> Value {
    if props.is_empty() {
        return Value::Null;
    }
    Value::List(
        props
            .iter()
            .map(|p| {
                let type_name: String = match p.value_type {
                    crate::state::StoredValueType::Int64 => "INT64".into(),
                    crate::state::StoredValueType::Float64 => "FLOAT64".into(),
                    crate::state::StoredValueType::Float32 => "FLOAT32".into(),
                    crate::state::StoredValueType::Text => "TEXT".into(),
                    crate::state::StoredValueType::Bool => "BOOL".into(),
                    crate::state::StoredValueType::Timestamp => "TIMESTAMP".into(),
                    crate::state::StoredValueType::List => "LIST".into(),
                    crate::state::StoredValueType::TypedList(s) => {
                        format!("LIST<{}>", stored_scalar_type_name(s))
                    }
                    crate::state::StoredValueType::Bytes => "BYTES".into(),
                    crate::state::StoredValueType::Date => "DATE".into(),
                    crate::state::StoredValueType::Time => "TIME".into(),
                    crate::state::StoredValueType::DateTime => "DATETIME".into(),
                    crate::state::StoredValueType::Duration => "DURATION".into(),
                    crate::state::StoredValueType::Decimal => "DECIMAL".into(),
                    crate::state::StoredValueType::Uint64 => "UINT64".into(),
                    crate::state::StoredValueType::Int8 => "INT8".into(),
                    crate::state::StoredValueType::Int16 => "INT16".into(),
                    crate::state::StoredValueType::Int32 => "INT32".into(),
                    crate::state::StoredValueType::Int128 => "INT128".into(),
                    crate::state::StoredValueType::Int256 => "INT256".into(),
                    crate::state::StoredValueType::Uint8 => "UINT8".into(),
                    crate::state::StoredValueType::Uint16 => "UINT16".into(),
                    crate::state::StoredValueType::Uint32 => "UINT32".into(),
                    crate::state::StoredValueType::Uint128 => "UINT128".into(),
                    crate::state::StoredValueType::Uint256 => "UINT256".into(),
                    crate::state::StoredValueType::TextConstrained {
                        min_length,
                        max_length,
                        fixed,
                    } => {
                        if fixed {
                            format!("CHAR({max_length})")
                        } else if min_length > 0 {
                            format!("STRING({min_length}, {max_length})")
                        } else {
                            format!("STRING({max_length})")
                        }
                    }
                    crate::state::StoredValueType::BytesConstrained {
                        min_length,
                        max_length,
                        fixed,
                    } => {
                        if fixed {
                            format!("BINARY({max_length})")
                        } else if min_length > 0 {
                            format!("BYTES({min_length}, {max_length})")
                        } else {
                            format!("BYTES({max_length})")
                        }
                    }
                };
                let s = if p.required {
                    format!("{}::{} NOT NULL", p.name, type_name)
                } else {
                    format!("{}::{}", p.name, type_name)
                };
                Value::Text(s)
            })
            .collect(),
    )
}

pub fn mutate_tracked(gql: &str) -> Result<MutationOutcome, GleaphError> {
    enforce_limits(gql)?;
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;

    // §16.2 USE GRAPH: USE GRAPH is read-only; reject in mutation context.
    if matches!(stmt, Statement::UseGraph(_)) {
        return Err(GleaphError::ValidationError(
            "USE GRAPH is a read-only operation; use the query endpoint".into(),
        ));
    }

    // Read-only statements: reject in mutation context.
    if matches!(stmt, Statement::DescribeGraphType(_) | Statement::Show(_)) {
        return Err(GleaphError::ValidationError(
            "this is a read-only operation; use the query endpoint".into(),
        ));
    }

    // §12 CREATE/DROP GRAPH: CREATE/DROP GRAPH require dedicated async endpoints.
    if matches!(
        stmt,
        Statement::CreateGraph { .. } | Statement::DropGraph { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "graph catalog operations require the execute_gql endpoint".into(),
        ));
    }

    // §12 GRAPH TYPE: CREATE/DROP GRAPH TYPE — handle locally.
    if let Some(outcome) = intercept_graph_type_ddl(&stmt)? {
        return Ok(outcome);
    }

    // §12 SCHEMA: CREATE/DROP SCHEMA — handle locally.
    if let Some(outcome) = intercept_schema_ddl(&stmt)? {
        return Ok(outcome);
    }

    // CREATE/DROP INDEX — handle locally.
    if let Some(outcome) = intercept_index_ddl(&stmt)? {
        return Ok(outcome);
    }

    // §12 CONSTRAINT: CREATE/DROP CONSTRAINT — handle locally.
    if let Some(outcome) = intercept_constraint_ddl(&stmt)? {
        return Ok(outcome);
    }

    // GRANT/REVOKE — handle locally.
    if let Some(outcome) = intercept_acl_ddl(&stmt)? {
        return Ok(outcome);
    }

    // ANALYZE — recompute planner statistics.
    if matches!(stmt, Statement::Analyze) {
        return intercept_analyze();
    }

    // §18.9 Phase 3: SET TYPE CHECK STRICT|WARNING.
    if let Statement::SetTypeCheck(mode) = &stmt {
        let strict = matches!(mode, gleaph_gql::ast::TypeCheckMode::Strict);
        crate::state::set_strict_type_check(strict);
        let label = if strict {
            "STRICT"
        } else {
            "WARNING"
        };
        return Ok(MutationOutcome {
            result: MutationResult {
                affected_vertices: 0,
                affected_edges: 0,
                warnings: vec![info_diagnostic(format!("type check mode set to {label}"))],
            },
            affected_vertex_ids: Vec::new(),
        });
    }

    enforce_quota_for_mutation(&stmt)?;
    enforce_graph_type_labels(&stmt)?;
    enforce_constraints(&stmt)?;
    // §12 Type annotations: Sync node type definitions for type annotation resolution.
    sync_node_type_defs();
    // Inject IC time and caller principal into executor thread-locals.
    set_current_time(ic_timestamp());
    let _caller_guard = inject_caller();
    struct ClearTimeGuardMut;
    impl Drop for ClearTimeGuardMut {
        fn drop(&mut self) {
            clear_current_time();
        }
    }
    let _clear_time = ClearTimeGuardMut;
    let ts = ic_timestamp();
    let outcome = with_state_mut(|g| {
        let max_execution_steps = match &stmt {
            Statement::Set(_) | Statement::Remove(_) => HARD_MAX_EXECUTION_STEPS_HEAVY_MUTATION,
            _ => HARD_MAX_EXECUTION_STEPS,
        };
        let outcome = execute_mutation_tracked(
            &stmt,
            g,
            ExecutionLimits {
                max_rows: Some(DEFAULT_MAX_ROWS),
                max_execution_steps: Some(max_execution_steps),
            },
            ts,
        );
        g.refresh_selectivity_if_stale();
        outcome
    });
    maybe_inject_test_fail_after_mutation_tracked(outcome)
}

pub fn mutate_resumable_with_params(
    gql: &str,
    params: &std::collections::HashMap<String, Value>,
    max_steps: u64,
) -> Result<MutationProgress, GleaphError> {
    use gleaph_gql::executor::{clear_query_params, set_query_params};
    set_query_params(params.clone());
    let result = mutate_resumable(gql, max_steps);
    clear_query_params();
    result
}

/// Executes a mutation with resumable support for DELETE.
/// Returns `MutationProgress::Suspended` with a checkpoint when the step budget is exceeded.
/// Also handles DDL statements (CREATE/DROP INDEX, GRAPH TYPE, SCHEMA, GRANT/REVOKE, ANALYZE, etc.).
pub fn mutate_resumable(gql: &str, max_steps: u64) -> Result<MutationProgress, GleaphError> {
    enforce_limits(gql)?;
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;

    // §16.2 USE GRAPH: read-only; reject in mutation context.
    if matches!(stmt, Statement::UseGraph(_)) {
        return Err(GleaphError::ValidationError(
            "USE GRAPH is a read-only operation; use the query endpoint".into(),
        ));
    }

    // Read-only statements: reject in mutation context.
    if matches!(stmt, Statement::DescribeGraphType(_) | Statement::Show(_)) {
        return Err(GleaphError::ValidationError(
            "this is a read-only operation; use the query endpoint".into(),
        ));
    }

    // §12 CREATE/DROP GRAPH: require dedicated async endpoints.
    if matches!(
        stmt,
        Statement::CreateGraph { .. } | Statement::DropGraph { .. }
    ) {
        return Err(GleaphError::ExecutionError(
            "graph catalog operations require the execute_gql endpoint".into(),
        ));
    }

    // §12 GRAPH TYPE: CREATE/DROP GRAPH TYPE — handle locally.
    if let Some(outcome) = intercept_graph_type_ddl(&stmt)? {
        return Ok(MutationProgress::Done(outcome));
    }

    // §12 SCHEMA: CREATE/DROP SCHEMA — handle locally.
    if let Some(outcome) = intercept_schema_ddl(&stmt)? {
        return Ok(MutationProgress::Done(outcome));
    }

    // CREATE/DROP INDEX — handle locally.
    if let Some(outcome) = intercept_index_ddl(&stmt)? {
        return Ok(MutationProgress::Done(outcome));
    }

    // §12 CONSTRAINT: CREATE/DROP CONSTRAINT — handle locally.
    if let Some(outcome) = intercept_constraint_ddl(&stmt)? {
        return Ok(MutationProgress::Done(outcome));
    }

    // GRANT/REVOKE — handle locally.
    if let Some(outcome) = intercept_acl_ddl(&stmt)? {
        return Ok(MutationProgress::Done(outcome));
    }

    // ANALYZE — recompute planner statistics.
    if matches!(stmt, Statement::Analyze) {
        return intercept_analyze().map(MutationProgress::Done);
    }

    // §18.9 Phase 3: SET TYPE CHECK STRICT|WARNING.
    if let Statement::SetTypeCheck(mode) = &stmt {
        let strict = matches!(mode, gleaph_gql::ast::TypeCheckMode::Strict);
        crate::state::set_strict_type_check(strict);
        let label = if strict {
            "STRICT"
        } else {
            "WARNING"
        };
        return Ok(MutationProgress::Done(MutationOutcome {
            result: MutationResult {
                affected_vertices: 0,
                affected_edges: 0,
                warnings: vec![info_diagnostic(format!("type check mode set to {label}"))],
            },
            affected_vertex_ids: Vec::new(),
        }));
    }

    enforce_quota_for_mutation(&stmt)?;
    enforce_graph_type_labels(&stmt)?;
    enforce_constraints(&stmt)?;
    sync_node_type_defs();
    set_current_time(ic_timestamp());
    let _caller_guard = inject_caller();
    struct ClearTimeGuard;
    impl Drop for ClearTimeGuard {
        fn drop(&mut self) {
            clear_current_time();
        }
    }
    let _clear_time = ClearTimeGuard;
    let ts = ic_timestamp();
    let progress = with_state_mut(|g| {
        execute_mutation_resumable(
            &stmt,
            g,
            ExecutionLimits {
                max_rows: Some(DEFAULT_MAX_ROWS),
                max_execution_steps: Some(max_steps),
            },
            ts,
        )
    });
    maybe_inject_test_fail_after_mutation_resumable(progress)
}

/// Resumes a suspended mutation from a checkpoint.
pub fn resume_mutation(
    checkpoint: gleaph_types::MutationCheckpoint,
    max_steps: u64,
) -> Result<MutationProgress, GleaphError> {
    with_state_mut(|g| {
        gleaph_gql::executor::resume_mutation(
            checkpoint,
            g,
            ExecutionLimits {
                max_rows: None,
                max_execution_steps: Some(max_steps),
            },
        )
    })
}

/// Count how many new vertices a batch of parsed statements will create.
/// Used to pre-expand the vertex array once before executing the batch.
fn count_create_vertices(stmts: &[Statement]) -> u32 {
    let mut count = 0u32;
    for stmt in stmts {
        match stmt {
            Statement::Create(cs) => {
                for c in cs {
                    match c {
                        CreateStmt::Node(_) => count += 1,
                        CreateStmt::Edge(_) => count += 2,
                    }
                }
            }
            Statement::Merge(m) => {
                // Merge creates vertices only on miss; conservatively pre-allocate.
                match &m.create {
                    CreateStmt::Node(_) => count += 1,
                    CreateStmt::Edge(_) => count += 2,
                }
            }
            _ => {}
        }
    }
    count
}

pub fn batch_mutate_tracked(gqls: &[String]) -> Vec<Result<MutationOutcome, GleaphError>> {
    let mut parsed = Vec::with_capacity(gqls.len());
    for gql in gqls {
        if let Err(e) = enforce_limits(gql) {
            return gqls.iter().map(|_| Err(e.clone())).collect();
        }
        let stmt = match parse_statement(gql) {
            Ok(stmt) => stmt,
            Err(e) => return gqls.iter().map(|_| Err(e.clone())).collect(),
        };
        if let Err(e) = validate_statement(&stmt) {
            return gqls.iter().map(|_| Err(e.clone())).collect();
        }
        parsed.push(stmt);
    }

    sync_node_type_defs();
    let ts = ic_timestamp();

    // Pre-expand vertex array for all CREATE/MERGE statements to avoid
    // repeated O(V+E) expand_vertices calls.
    let vertex_count = count_create_vertices(&parsed);
    if vertex_count > 0
        && let Err(e) = with_state_mut(|g| g.reserve_vertices(vertex_count))
    {
        return parsed.iter().map(|_| Err(e.clone())).collect();
    }

    let results: Vec<Result<MutationOutcome, GleaphError>> = parsed
        .iter()
        .map(|stmt| {
            enforce_quota_for_mutation(stmt)?;
            enforce_graph_type_labels(stmt)?;
            enforce_constraints(stmt)?;
            if let Some(outcome) = intercept_graph_type_ddl(stmt)? {
                return Ok(outcome);
            }
            if let Some(outcome) = intercept_schema_ddl(stmt)? {
                return Ok(outcome);
            }
            if let Some(outcome) = intercept_constraint_ddl(stmt)? {
                return Ok(outcome);
            }
            let outcome = with_state_mut(|g| {
                let max_execution_steps = match stmt {
                    Statement::Set(_) | Statement::Remove(_) => {
                        HARD_MAX_EXECUTION_STEPS_HEAVY_MUTATION
                    }
                    _ => HARD_MAX_EXECUTION_STEPS,
                };
                execute_mutation_tracked(
                    stmt,
                    g,
                    ExecutionLimits {
                        max_rows: Some(DEFAULT_MAX_ROWS),
                        max_execution_steps: Some(max_execution_steps),
                    },
                    ts,
                )
            });
            maybe_inject_test_fail_after_mutation_tracked(outcome)
        })
        .collect();
    with_state_mut(|g| g.refresh_selectivity_if_stale());
    results
}

pub fn batch_mutate_tracked_with_params(
    gqls: &[(String, std::collections::HashMap<String, Value>)],
) -> Vec<Result<MutationOutcome, GleaphError>> {
    use gleaph_gql::executor::{clear_query_params, set_query_params};
    let mut parsed = Vec::with_capacity(gqls.len());
    for (gql, _) in gqls {
        if let Err(e) = enforce_limits(gql) {
            return gqls.iter().map(|_| Err(e.clone())).collect();
        }
        let stmt = match parse_statement(gql) {
            Ok(stmt) => stmt,
            Err(e) => return gqls.iter().map(|_| Err(e.clone())).collect(),
        };
        if let Err(e) = validate_statement(&stmt) {
            return gqls.iter().map(|_| Err(e.clone())).collect();
        }
        parsed.push(stmt);
    }

    sync_node_type_defs();
    let ts = ic_timestamp();
    let vertex_count = count_create_vertices(&parsed);
    if vertex_count > 0
        && let Err(e) = with_state_mut(|g| g.reserve_vertices(vertex_count))
    {
        return parsed.iter().map(|_| Err(e.clone())).collect();
    }

    let results: Vec<Result<MutationOutcome, GleaphError>> = parsed
        .iter()
        .zip(gqls.iter())
        .map(|(stmt, (_, params))| {
            enforce_quota_for_mutation(stmt)?;
            enforce_graph_type_labels(stmt)?;
            enforce_constraints(stmt)?;
            if let Some(outcome) = intercept_graph_type_ddl(stmt)? {
                return Ok(outcome);
            }
            if let Some(outcome) = intercept_schema_ddl(stmt)? {
                return Ok(outcome);
            }
            if let Some(outcome) = intercept_constraint_ddl(stmt)? {
                return Ok(outcome);
            }
            set_query_params(params.clone());
            let outcome = with_state_mut(|g| {
                let max_execution_steps = match stmt {
                    Statement::Set(_) | Statement::Remove(_) => {
                        HARD_MAX_EXECUTION_STEPS_HEAVY_MUTATION
                    }
                    _ => HARD_MAX_EXECUTION_STEPS,
                };
                execute_mutation_tracked(
                    stmt,
                    g,
                    ExecutionLimits {
                        max_rows: Some(DEFAULT_MAX_ROWS),
                        max_execution_steps: Some(max_execution_steps),
                    },
                    ts,
                )
            });
            clear_query_params();
            maybe_inject_test_fail_after_mutation_tracked(outcome)
        })
        .collect();
    with_state_mut(|g| g.refresh_selectivity_if_stale());
    results
}

#[allow(dead_code)]
pub fn batch_mutate(gqls: &[String]) -> Vec<Result<MutationResult, GleaphError>> {
    batch_mutate_tracked(gqls)
        .into_iter()
        .map(|o| o.map(|out| out.result))
        .collect()
}

#[cfg(test)]
pub(crate) fn arm_test_fail_after_mutation_once() {
    TEST_FAIL_AFTER_MUTATION_ONCE.with(|f| f.set(true));
}

#[cfg(not(test))]
fn maybe_inject_test_fail_after_mutation_tracked(
    result: Result<MutationOutcome, GleaphError>,
) -> Result<MutationOutcome, GleaphError> {
    result
}

#[cfg(test)]
fn maybe_inject_test_fail_after_mutation_tracked(
    result: Result<MutationOutcome, GleaphError>,
) -> Result<MutationOutcome, GleaphError> {
    if result.is_ok()
        && TEST_FAIL_AFTER_MUTATION_ONCE.with(|f| {
            let armed = f.get();
            if armed {
                f.set(false);
            }
            armed
        })
    {
        return Err(GleaphError::ExecutionError(
            "test failpoint: error after mutation commit".into(),
        ));
    }
    result
}

#[cfg(not(test))]
fn maybe_inject_test_fail_after_mutation_resumable(
    result: Result<MutationProgress, GleaphError>,
) -> Result<MutationProgress, GleaphError> {
    result
}

#[cfg(test)]
fn maybe_inject_test_fail_after_mutation_resumable(
    result: Result<MutationProgress, GleaphError>,
) -> Result<MutationProgress, GleaphError> {
    if let Ok(MutationProgress::Done(_)) = &result {
        if TEST_FAIL_AFTER_MUTATION_ONCE.with(|f| {
            let armed = f.get();
            if armed {
                f.set(false);
            }
            armed
        }) {
            return Err(GleaphError::ExecutionError(
                "test failpoint: error after mutation commit".into(),
            ));
        }
    }
    result
}

/// §12: Intercepts CREATE/DROP GRAPH TYPE statements and handles them locally.
///
/// Returns `Some(MutationOutcome)` if the statement was handled, `None` otherwise.
fn intercept_graph_type_ddl(stmt: &Statement) -> Result<Option<MutationOutcome>, GleaphError> {
    match stmt {
        Statement::CreateGraphType {
            name,
            definition,
            if_not_exists,
            or_replace,
            source,
        } => {
            // LIKE / COPY OF: resolve from existing graph type
            if let Some(src_name) = source {
                let src = crate::state::get_graph_type(src_name).ok_or_else(|| {
                    GleaphError::ExecutionError(format!(
                        "source graph type '{src_name}' does not exist"
                    ))
                })?;
                if *if_not_exists && !*or_replace && crate::state::get_graph_type(name).is_some() {
                    return Ok(Some(MutationOutcome {
                        result: MutationResult {
                            affected_vertices: 0,
                            affected_edges: 0,
                            warnings: vec![info_diagnostic(format!(
                                "graph type '{name}' already exists, skipped"
                            ))],
                        },
                        affected_vertex_ids: Vec::new(),
                    }));
                }
                crate::state::set_graph_type(name.clone(), src);
                crate::state::set_active_graph_type(Some(name.clone()));
                return Ok(Some(MutationOutcome {
                    result: MutationResult {
                        affected_vertices: 0,
                        affected_edges: 0,
                        warnings: vec![info_diagnostic(format!(
                            "graph type '{name}' created from '{src_name}' and activated"
                        ))],
                    },
                    affected_vertex_ids: Vec::new(),
                }));
            }
            if *if_not_exists && !*or_replace && crate::state::get_graph_type(name).is_some() {
                return Ok(Some(MutationOutcome {
                    result: MutationResult {
                        affected_vertices: 0,
                        affected_edges: 0,
                        warnings: vec![info_diagnostic(format!(
                            "graph type '{name}' already exists, skipped"
                        ))],
                    },
                    affected_vertex_ids: Vec::new(),
                }));
            }
            let stored = crate::state::StoredGraphType {
                node_labels: definition.node_labels.clone(),
                edge_labels: definition.edge_labels.clone(),
                node_types: definition
                    .node_types
                    .iter()
                    .map(|nt| crate::state::StoredNodeType {
                        name: nt.name.clone(),
                        labels: nt.labels.clone(),
                        properties: nt
                            .properties
                            .iter()
                            .map(|p| crate::state::StoredPropertyDef {
                                name: p.name.clone(),
                                value_type: ast_value_type_to_stored(p.value_type),
                                required: p.required,
                            })
                            .collect(),
                    })
                    .collect(),
                edge_types: definition
                    .edge_types
                    .iter()
                    .map(|et| crate::state::StoredEdgeType {
                        name: et.name.clone(),
                        label: et.label.clone(),
                        from_types: et.from_labels.clone(),
                        to_types: et.to_labels.clone(),
                        properties: et
                            .properties
                            .iter()
                            .map(|p| crate::state::StoredPropertyDef {
                                name: p.name.clone(),
                                value_type: ast_value_type_to_stored(p.value_type),
                                required: p.required,
                            })
                            .collect(),
                    })
                    .collect(),
            };
            crate::state::set_graph_type(name.clone(), stored);
            crate::state::set_active_graph_type(Some(name.clone()));
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!(
                        "graph type '{name}' created and activated"
                    ))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        Statement::DropGraphType { name, if_exists } => {
            if !crate::state::remove_graph_type(name) {
                if *if_exists {
                    return Ok(Some(MutationOutcome {
                        result: MutationResult {
                            affected_vertices: 0,
                            affected_edges: 0,
                            warnings: vec![info_diagnostic(format!(
                                "graph type '{name}' does not exist, skipped"
                            ))],
                        },
                        affected_vertex_ids: Vec::new(),
                    }));
                }
                return Err(GleaphError::ExecutionError(format!(
                    "graph type '{name}' does not exist"
                )));
            }
            // If the dropped type was the active one, deactivate.
            if crate::state::get_active_graph_type_name().as_deref() == Some(name.as_str()) {
                crate::state::set_active_graph_type(None);
            }
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!("graph type '{name}' dropped"))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        _ => Ok(None),
    }
}

/// §12: Intercepts CREATE/DROP SCHEMA statements and handles them locally.
fn intercept_schema_ddl(stmt: &Statement) -> Result<Option<MutationOutcome>, GleaphError> {
    match stmt {
        Statement::CreateSchema {
            name,
            if_not_exists,
        } => {
            if !crate::state::create_schema(name.clone()) {
                if *if_not_exists {
                    return Ok(Some(MutationOutcome {
                        result: MutationResult {
                            affected_vertices: 0,
                            affected_edges: 0,
                            warnings: vec![info_diagnostic(format!(
                                "schema '{name}' already exists, skipped"
                            ))],
                        },
                        affected_vertex_ids: Vec::new(),
                    }));
                }
                return Err(GleaphError::ExecutionError(format!(
                    "schema '{name}' already exists"
                )));
            }
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!("schema '{name}' created"))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        Statement::DropSchema { name, if_exists } => {
            if !crate::state::drop_schema(name) {
                if *if_exists {
                    return Ok(Some(MutationOutcome {
                        result: MutationResult {
                            affected_vertices: 0,
                            affected_edges: 0,
                            warnings: vec![info_diagnostic(format!(
                                "schema '{name}' does not exist, skipped"
                            ))],
                        },
                        affected_vertex_ids: Vec::new(),
                    }));
                }
                return Err(GleaphError::ExecutionError(format!(
                    "schema '{name}' does not exist"
                )));
            }
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!("schema '{name}' dropped"))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        _ => Ok(None),
    }
}

/// Returns `true` if the statement is a write operation (DML or DDL) that should
/// be rejected in a read-only query context.
fn is_mutation_only_statement(stmt: &Statement) -> bool {
    matches!(
        stmt,
        // DML
        Statement::Create(_)
            | Statement::Merge(_)
            | Statement::Delete(_)
            | Statement::Set(_)
            | Statement::Remove(_)
            // DDL
            | Statement::CreateGraphType { .. }
            | Statement::DropGraphType { .. }
            | Statement::CreateSchema { .. }
            | Statement::DropSchema { .. }
            | Statement::CreateIndex { .. }
            | Statement::DropIndex { .. }
            | Statement::Grant { .. }
            | Statement::Revoke { .. }
            | Statement::Analyze
            | Statement::SetTypeCheck(_)
            | Statement::CreateConstraint(_)
            | Statement::DropConstraint(_)
    )
}

/// Resolve `SHOW ...` statements into a `QueryResult` table.
fn resolve_show(target: &ShowTarget) -> Result<QueryResult, GleaphError> {
    let empty_stats = || gleaph_types::QueryStats {
        scanned_vertices: 0,
        scanned_edges: 0,
        rows_emitted: 0,
        execution_steps: 0,
        breakdown: Default::default(),
    };
    match target {
        ShowTarget::Stats => {
            let stats = with_state(|g| g.stats());
            Ok(QueryResult {
                columns: vec![
                    "num_vertices".into(),
                    "num_edges".into(),
                    "elem_capacity".into(),
                    "segment_size".into(),
                    "segment_count".into(),
                    "avg_degree".into(),
                ],
                rows: vec![vec![
                    Value::Int64(stats.num_vertices as i64),
                    Value::Int64(stats.num_edges as i64),
                    Value::Int64(stats.elem_capacity as i64),
                    Value::Int64(stats.segment_size as i64),
                    Value::Int64(stats.segment_count as i64),
                    Value::Float64(stats.avg_degree),
                ]],
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::PlannerStats => {
            let ps = with_state(|g| g.planner_stats());
            let mut rows = Vec::new();
            // One row per label cardinality
            for (label, count) in &ps.label_cardinality {
                rows.push(vec![
                    Value::Text(format!("label:{label}")),
                    Value::Int64(*count as i64),
                    Value::Float64(0.0),
                ]);
            }
            // One row per selectivity
            for (prop, sel) in &ps.property_selectivity {
                rows.push(vec![
                    Value::Text(format!("selectivity:{prop}")),
                    Value::Int64(0),
                    Value::Float64(*sel),
                ]);
            }
            // Summary row
            rows.push(vec![
                Value::Text("summary".into()),
                Value::Int64(ps.vertex_count as i64),
                Value::Float64(ps.avg_degree),
            ]);
            Ok(QueryResult {
                columns: vec!["key".into(), "value".into(), "float_value".into()],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Indexes => {
            let indexes = with_state(|g| g.list_property_indexes());
            let rows: Vec<Vec<Value>> = indexes
                .iter()
                .map(|idx| {
                    vec![
                        Value::Text(match idx.entity_type {
                            EntityType::Vertex => "Vertex".into(),
                            EntityType::Edge => "Edge".into(),
                        }),
                        Value::Text(idx.property_name.clone()),
                        Value::Text(match idx.index_type {
                            IndexType::Equality => "Equality".into(),
                            IndexType::Range => "Range".into(),
                        }),
                    ]
                })
                .collect();
            Ok(QueryResult {
                columns: vec![
                    "entity_type".into(),
                    "property_name".into(),
                    "index_type".into(),
                ],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Grants => {
            let entries = crate::state::list_acl_entries();
            let rows: Vec<Vec<Value>> = entries
                .iter()
                .map(|e| {
                    vec![
                        Value::Text(e.principal.to_text()),
                        Value::Text(format!("{:?}", e.level)),
                    ]
                })
                .collect();
            Ok(QueryResult {
                columns: vec!["principal".into(), "level".into()],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Metrics => {
            let m = crate::state::with_metrics(|m| m.clone());
            Ok(QueryResult {
                columns: vec![
                    "query_count".into(),
                    "mutation_count".into(),
                    "rejected_count".into(),
                    "algorithm_calls".into(),
                    "stable_memory_bytes".into(),
                ],
                rows: vec![vec![
                    Value::Int64(m.query_count as i64),
                    Value::Int64(m.mutation_count as i64),
                    Value::Int64(m.rejected_count as i64),
                    Value::Int64(m.algorithm_calls as i64),
                    Value::Int64(m.stable_memory_bytes as i64),
                ]],
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Schemas => {
            let schemas = crate::state::list_schemas();
            let rows: Vec<Vec<Value>> = schemas.into_iter().map(|s| vec![Value::Text(s)]).collect();
            Ok(QueryResult {
                columns: vec!["schema_name".into()],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::GraphTypes => {
            let types = crate::state::list_graph_types();
            let rows: Vec<Vec<Value>> = types
                .iter()
                .map(|(name, gt)| {
                    vec![
                        Value::Text(name.clone()),
                        Value::Int64(gt.node_labels.len() as i64),
                        Value::Int64(gt.edge_labels.len() as i64),
                    ]
                })
                .collect();
            Ok(QueryResult {
                columns: vec![
                    "name".into(),
                    "node_label_count".into(),
                    "edge_label_count".into(),
                ],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Quota => {
            let q = crate::state::get_quota();
            Ok(QueryResult {
                columns: vec!["max_vertices".into(), "max_edges".into()],
                rows: vec![vec![
                    Value::Int64(q.max_vertices as i64),
                    Value::Int64(q.max_edges as i64),
                ]],
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Aliases => {
            let aliases = crate::state::list_graph_aliases();
            let rows: Vec<Vec<Value>> = aliases
                .iter()
                .map(|a| {
                    vec![
                        Value::Text(a.name.clone()),
                        Value::Text(a.canister_id.to_text()),
                    ]
                })
                .collect();
            Ok(QueryResult {
                columns: vec!["name".into(), "canister_id".into()],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Prepared => {
            let stmts = crate::state::list_prepared_stmts();
            let rows: Vec<Vec<Value>> = stmts
                .into_iter()
                .map(|(name, ps)| vec![Value::Text(name), Value::Text(ps.source)])
                .collect();
            Ok(QueryResult {
                columns: vec!["name".into(), "source".into()],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Constraints => {
            let constraints = crate::state::list_constraints();
            let rows = constraints
                .iter()
                .map(|c| {
                    vec![
                        Value::Text(c.name.clone()),
                        Value::Text(
                            match c.kind {
                                crate::state::StoredConstraintKind::Unique => "UNIQUE",
                                crate::state::StoredConstraintKind::NotNull => "NOT NULL",
                            }
                            .into(),
                        ),
                        Value::Text(c.label.clone()),
                        Value::Text(c.property.clone()),
                    ]
                })
                .collect();
            Ok(QueryResult {
                columns: vec![
                    "name".into(),
                    "kind".into(),
                    "label".into(),
                    "property".into(),
                ],
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        ShowTarget::Settings => {
            let type_check_mode = if crate::state::is_strict_type_check() {
                "STRICT"
            } else {
                "WARNING"
            };
            Ok(QueryResult {
                columns: vec!["setting".into(), "value".into()],
                rows: vec![vec![
                    Value::Text("type_check_mode".into()),
                    Value::Text(type_check_mode.into()),
                ]],
                stats: empty_stats(),
                warnings: vec![],
            })
        }
    }
}

/// Execute a `CALL procedure(args) YIELD cols` built-in algorithm invocation.
fn execute_call_procedure(
    call: &gleaph_gql::ast::CallProcedureStmt,
) -> Result<QueryResult, GleaphError> {
    use gleaph_gql::ast::Expr;

    let empty_stats = || gleaph_types::QueryStats {
        scanned_vertices: 0,
        scanned_edges: 0,
        rows_emitted: 0,
        execution_steps: 0,
        breakdown: Default::default(),
    };

    /// Evaluate a constant expression (literal, parameter, arithmetic, list, record).
    ///
    /// CALL procedure arguments are evaluated before any MATCH, so variable
    /// references and property accesses are not available.
    fn eval_const_expr(expr: &Expr) -> Result<Value, GleaphError> {
        match expr {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Parameter { name, .. } => {
                let val = gleaph_gql::executor::get_query_param(name);
                Ok(val.unwrap_or(Value::Null))
            }
            Expr::UnaryOp { op, expr } => {
                let v = eval_const_expr(expr)?;
                match op {
                    gleaph_gql::ast::UnaryOp::Neg => match v {
                        Value::Int64(i) => Ok(Value::Int64(-i)),
                        Value::Float64(f) => Ok(Value::Float64(-f)),
                        _ => Err(GleaphError::ExecutionError(
                            "negation only supported for numeric values".into(),
                        )),
                    },
                    gleaph_gql::ast::UnaryOp::Pos => match v {
                        Value::Int64(_) | Value::Float64(_) => Ok(v),
                        _ => Err(GleaphError::ExecutionError(
                            "unary plus only supported for numeric values".into(),
                        )),
                    },
                }
            }
            Expr::BinaryOp { left, op, right } => {
                let l = eval_const_expr(left)?;
                let r = eval_const_expr(right)?;
                use gleaph_gql::ast::BinaryOp;
                match (op, &l, &r) {
                    (BinaryOp::Add, Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64(a + b)),
                    (BinaryOp::Sub, Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64(a - b)),
                    (BinaryOp::Mul, Value::Int64(a), Value::Int64(b)) => Ok(Value::Int64(a * b)),
                    (BinaryOp::Div, Value::Int64(a), Value::Int64(b)) if *b != 0 => {
                        Ok(Value::Int64(a / b))
                    }
                    (BinaryOp::Mod, Value::Int64(a), Value::Int64(b)) if *b != 0 => {
                        Ok(Value::Int64(a % b))
                    }
                    (BinaryOp::Add, Value::Float64(a), Value::Float64(b)) => {
                        Ok(Value::Float64(a + b))
                    }
                    (BinaryOp::Sub, Value::Float64(a), Value::Float64(b)) => {
                        Ok(Value::Float64(a - b))
                    }
                    (BinaryOp::Mul, Value::Float64(a), Value::Float64(b)) => {
                        Ok(Value::Float64(a * b))
                    }
                    (BinaryOp::Div, Value::Float64(a), Value::Float64(b)) if *b != 0.0 => {
                        Ok(Value::Float64(a / b))
                    }
                    (BinaryOp::Add, Value::Text(a), Value::Text(b)) => {
                        Ok(Value::Text(format!("{a}{b}")))
                    }
                    _ => Err(GleaphError::ExecutionError(format!(
                        "unsupported operation {op:?} on CALL procedure arguments"
                    ))),
                }
            }
            Expr::RecordLiteral(entries) => {
                let mut pairs = Vec::new();
                for (k, v) in entries {
                    pairs.push(Value::List(vec![
                        Value::Text(k.clone()),
                        eval_const_expr(v)?,
                    ]));
                }
                Ok(Value::List(pairs))
            }
            Expr::ListLiteral(items) => {
                let mut vals = Vec::with_capacity(items.len());
                for item in items {
                    vals.push(eval_const_expr(item)?);
                }
                Ok(Value::List(vals))
            }
            _ => Err(GleaphError::ExecutionError(
                "CALL procedure arguments must be constant expressions \
                 (literals, $parameters, arithmetic, records, lists)"
                    .into(),
            )),
        }
    }

    /// Extract a named field from a map Value (list of [key, value] pairs).
    fn map_get(map: &Value, key: &str) -> Option<Value> {
        if let Value::List(pairs) = map {
            for pair in pairs {
                if let Value::List(kv) = pair
                    && kv.len() == 2
                    && let Value::Text(k) = &kv[0]
                    && k == key
                {
                    return Some(kv[1].clone());
                }
            }
        }
        None
    }

    fn as_u32(v: &Value) -> Result<u32, GleaphError> {
        if let Some(i) = v.as_i64() {
            Ok(i as u32)
        } else {
            Err(GleaphError::ExecutionError(
                "expected integer argument".into(),
            ))
        }
    }

    fn as_opt_u32(map: &Value, key: &str) -> Option<u32> {
        map_get(map, key).and_then(|v| v.as_i64().map(|i| i as u32))
    }

    fn as_opt_u64(map: &Value, key: &str) -> Option<u64> {
        map_get(map, key).and_then(|v| v.as_i64().map(|i| i as u64))
    }

    fn as_opt_f64(map: &Value, key: &str) -> Option<f64> {
        map_get(map, key).and_then(|v| match v {
            Value::Float64(f) => Some(f),
            other => other.as_i64().map(|i| i as f64),
        })
    }

    fn as_opt_string(map: &Value, key: &str) -> Option<String> {
        map_get(map, key).and_then(|v| match v {
            Value::Text(s) => Some(s),
            _ => None,
        })
    }

    fn as_opt_bool(map: &Value, key: &str) -> Option<bool> {
        map_get(map, key).and_then(|v| match v {
            Value::Bool(b) => Some(b),
            _ => None,
        })
    }

    /// Resolve YIELD columns: if Some, validate against valid_cols; if None, use all valid_cols.
    fn resolve_yield(
        yield_cols: &Option<Vec<String>>,
        valid_cols: &[&str],
        proc_name: &str,
    ) -> Result<Vec<String>, GleaphError> {
        match yield_cols {
            Some(cols) => {
                for col in cols {
                    if !valid_cols.contains(&col.as_str()) {
                        return Err(GleaphError::ExecutionError(format!(
                            "{proc_name} does not yield column '{col}'; available: {valid_cols:?}"
                        )));
                    }
                }
                Ok(cols.clone())
            }
            None => Ok(valid_cols.iter().map(|s| s.to_string()).collect()),
        }
    }

    match call.procedure.to_ascii_lowercase().as_str() {
        "bfs" => {
            // CALL bfs(start, { max_depth: N, edge_label: 'L', ... }) YIELD vertex_id, distance
            if call.args.is_empty() {
                return Err(GleaphError::ExecutionError(
                    "bfs requires at least 1 argument: start vertex id".into(),
                ));
            }
            let start = as_u32(&eval_const_expr(&call.args[0])?)?;
            let config_val = if call.args.len() > 1 {
                eval_const_expr(&call.args[1])?
            } else {
                Value::List(vec![])
            };
            let config = gleaph_algo::bfs::BfsConfig {
                max_depth: as_opt_u32(&config_val, "max_depth"),
                max_visited: as_opt_u64(&config_val, "max_visited").map(|v| v as usize),
                target: as_opt_u32(&config_val, "target"),
                edge_label: as_opt_string(&config_val, "edge_label"),
                edge_label_expr: None,
                ts_range: None,
            };
            let result = with_state(|g| {
                let mut budget = gleaph_algo::budget::UnlimitedBudget;
                gleaph_algo::bfs::bfs(g, start, &config, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();

            // Build rows based on YIELD columns
            let cols = resolve_yield(&call.yield_cols, &["vertex_id", "distance"], "bfs")?;
            let rows: Vec<Vec<Value>> = result
                .distances
                .iter()
                .map(|(vid, dist)| {
                    cols.iter()
                        .map(|col| match col.as_str() {
                            "vertex_id" => Value::Int64(*vid as i64),
                            "distance" => Value::Int64(*dist as i64),
                            _ => Value::Null,
                        })
                        .collect()
                })
                .collect();
            Ok(QueryResult {
                columns: cols,
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        "sssp" => {
            // CALL sssp(start, { max_distance: N, ... }) YIELD vertex_id, distance, predecessor
            if call.args.is_empty() {
                return Err(GleaphError::ExecutionError(
                    "sssp requires at least 1 argument: start vertex id".into(),
                ));
            }
            let start = as_u32(&eval_const_expr(&call.args[0])?)?;
            let config_val = if call.args.len() > 1 {
                eval_const_expr(&call.args[1])?
            } else {
                Value::List(vec![])
            };
            let config = gleaph_algo::sssp::SsspConfig {
                max_distance: as_opt_f64(&config_val, "max_distance"),
                max_visited: as_opt_u64(&config_val, "max_visited").map(|v| v as usize),
                target: as_opt_u32(&config_val, "target"),
                edge_label: as_opt_string(&config_val, "edge_label"),
                ts_range: None,
            };
            let result = with_state(|g| {
                let mut budget = gleaph_algo::budget::UnlimitedBudget;
                gleaph_algo::sssp::dijkstra(g, start, &config, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();

            let cols = resolve_yield(
                &call.yield_cols,
                &["vertex_id", "distance", "predecessor"],
                "sssp",
            )?;
            // Build a lookup map for predecessors
            let pred_map: std::collections::HashMap<u32, Option<u32>> =
                result.predecessors.iter().cloned().collect();
            let rows: Vec<Vec<Value>> = result
                .distances
                .iter()
                .map(|(vid, dist)| {
                    cols.iter()
                        .map(|col| match col.as_str() {
                            "vertex_id" => Value::Int64(*vid as i64),
                            "distance" => Value::Float64(*dist),
                            "predecessor" => pred_map
                                .get(vid)
                                .and_then(|p| p.map(|v| Value::Int64(v as i64)))
                                .unwrap_or(Value::Null),
                            _ => Value::Null,
                        })
                        .collect()
                })
                .collect();
            Ok(QueryResult {
                columns: cols,
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        "pagerank" => {
            // CALL pagerank({ damping: 0.85, max_iterations: 100, ... }) YIELD vertex_id, score
            let config_val = if !call.args.is_empty() {
                eval_const_expr(&call.args[0])?
            } else {
                Value::List(vec![])
            };
            let config = gleaph_algo::pagerank::PageRankConfig {
                damping: as_opt_f64(&config_val, "damping").unwrap_or(0.85),
                max_iterations: as_opt_u32(&config_val, "max_iterations").unwrap_or(20),
                convergence_threshold: as_opt_f64(&config_val, "convergence_threshold")
                    .unwrap_or(1e-6),
                ts_range: None,
            };
            let result = with_state(|g| {
                let mut budget = gleaph_algo::budget::UnlimitedBudget;
                gleaph_algo::pagerank::pagerank(g, &config, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();

            let cols = resolve_yield(
                &call.yield_cols,
                &["vertex_id", "score", "iterations", "converged"],
                "pagerank",
            )?;
            let rows: Vec<Vec<Value>> = result
                .scores
                .iter()
                .map(|(vid, score)| {
                    cols.iter()
                        .map(|col| match col.as_str() {
                            "vertex_id" => Value::Int64(*vid as i64),
                            "score" => Value::Float64(*score),
                            "iterations" => Value::Int64(result.iterations as i64),
                            "converged" => Value::Bool(result.converged),
                            _ => Value::Null,
                        })
                        .collect()
                })
                .collect();
            Ok(QueryResult {
                columns: cols,
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        "recommend" => {
            // CALL recommend(user, { edge_label: 'L', max_hops: N, limit: N, ... })
            //     YIELD vertex_id, score, path
            if call.args.is_empty() {
                return Err(GleaphError::ExecutionError(
                    "recommend requires at least 1 argument: user vertex id".into(),
                ));
            }
            let user = as_u32(&eval_const_expr(&call.args[0])?)?;
            let config_val = if call.args.len() > 1 {
                eval_const_expr(&call.args[1])?
            } else {
                Value::List(vec![])
            };
            let edge_label = as_opt_string(&config_val, "edge_label").ok_or_else(|| {
                GleaphError::ExecutionError("recommend requires 'edge_label' in config".into())
            })?;
            let config = gleaph_algo::recommend::RecommendConfig {
                edge_label,
                max_hops: as_opt_u32(&config_val, "max_hops").unwrap_or(2) as u8,
                limit: as_opt_u32(&config_val, "limit").unwrap_or(20),
                ts_range: None,
                exclude_known: as_opt_bool(&config_val, "exclude_known").unwrap_or(true),
            };
            let results = with_state(|g| {
                let mut budget = gleaph_algo::budget::UnlimitedBudget;
                gleaph_algo::recommend::recommend(g, user, &config, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();

            let cols = resolve_yield(
                &call.yield_cols,
                &["vertex_id", "score", "path"],
                "recommend",
            )?;
            let rows: Vec<Vec<Value>> = results
                .iter()
                .map(|rec| {
                    cols.iter()
                        .map(|col| match col.as_str() {
                            "vertex_id" => Value::Int64(rec.vertex_id as i64),
                            "score" => Value::Float64(rec.score),
                            "path" => Value::List(
                                rec.path.iter().map(|v| Value::Int64(*v as i64)).collect(),
                            ),
                            _ => Value::Null,
                        })
                        .collect()
                })
                .collect();
            Ok(QueryResult {
                columns: cols,
                rows,
                stats: empty_stats(),
                warnings: vec![],
            })
        }
        other => Err(GleaphError::ExecutionError(format!(
            "unknown procedure '{other}'; available: bfs, sssp, pagerank, recommend"
        ))),
    }
}

/// Intercept `CREATE/DROP INDEX` statements.
fn intercept_index_ddl(stmt: &Statement) -> Result<Option<MutationOutcome>, GleaphError> {
    match stmt {
        Statement::CreateIndex {
            entity_type,
            property_name,
        } => {
            with_state_mut(|g| {
                g.create_index(*entity_type, property_name.clone(), IndexType::Equality)?;
                // Immediately compute selectivity for the newly indexed property so
                // the planner can use it without requiring a separate ANALYZE.
                let prefix = match entity_type {
                    EntityType::Vertex => "vertex",
                    EntityType::Edge => "edge",
                };
                g.compute_selectivity_for_properties(&[format!("{prefix}:{property_name}")]);
                Ok(())
            })?;
            if matches!(entity_type, EntityType::Vertex) {
                crate::state::ensure_secondary_index_reserved_region_initialized(0)?;
                crate::state::rebuild_secondary_index_abp_snapshot()?;
            }
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!(
                        "index on {entity_type:?}({property_name}) created"
                    ))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        Statement::DropIndex {
            entity_type,
            property_name,
        } => {
            with_state_mut(|g| {
                g.drop_index(*entity_type, property_name.clone(), IndexType::Equality)
            })?;
            // Rebuild ABP snapshot to remove stale entries for the dropped index.
            if matches!(entity_type, EntityType::Vertex) {
                let _ = crate::state::rebuild_secondary_index_abp_snapshot();
            }
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!(
                        "index on {entity_type:?}({property_name}) dropped"
                    ))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        _ => Ok(None),
    }
}

/// Intercept `GRANT/REVOKE` ACL statements.
fn intercept_acl_ddl(stmt: &Statement) -> Result<Option<MutationOutcome>, GleaphError> {
    match stmt {
        Statement::Grant { level, principal } => {
            // ACL operations require Admin permission — check is done in api.rs layer
            // but here we operate at bridge level. The api.rs check_caller_permission
            // is called before mutate_tracked. We'll handle the principal parsing here.
            let p = candid::Principal::from_text(principal).map_err(|e| {
                GleaphError::ExecutionError(format!("invalid principal '{principal}': {e}"))
            })?;
            crate::state::set_acl_entry(p, level.clone());
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!(
                        "granted {level:?} access to {principal}"
                    ))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        Statement::Revoke { principal } => {
            let p = candid::Principal::from_text(principal).map_err(|e| {
                GleaphError::ExecutionError(format!("invalid principal '{principal}': {e}"))
            })?;
            crate::state::remove_acl_entry(&p);
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!("revoked access from {principal}"))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        _ => Ok(None),
    }
}

/// Handle `ANALYZE` — recompute planner statistics.
fn intercept_analyze() -> Result<MutationOutcome, GleaphError> {
    with_state_mut(|g| g.refresh_selectivity_if_stale());
    Ok(MutationOutcome {
        result: MutationResult {
            affected_vertices: 0,
            affected_edges: 0,
            warnings: vec![info_diagnostic("planner statistics refreshed")],
        },
        affected_vertex_ids: Vec::new(),
    })
}

/// Intercept `CREATE/DROP CONSTRAINT` statements.
fn intercept_constraint_ddl(stmt: &Statement) -> Result<Option<MutationOutcome>, GleaphError> {
    match stmt {
        Statement::CreateConstraint(def) => {
            use crate::state::{StoredConstraint, StoredConstraintKind};
            if crate::state::get_constraint(&def.name).is_some() {
                return Err(GleaphError::ExecutionError(format!(
                    "constraint '{}' already exists",
                    def.name
                )));
            }
            let kind = match def.kind {
                gleaph_gql::ast::ConstraintKind::Unique => StoredConstraintKind::Unique,
                gleaph_gql::ast::ConstraintKind::NotNull => StoredConstraintKind::NotNull,
            };
            // Validate existing data against the new constraint.
            validate_constraint_against_existing_data(&def.label, &def.property, kind)?;
            let stored = StoredConstraint {
                name: def.name.clone(),
                label: def.label.clone(),
                property: def.property.clone(),
                kind,
            };
            crate::state::set_constraint(def.name.clone(), stored);
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!(
                        "constraint '{}' created",
                        def.name
                    ))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        Statement::DropConstraint(name) => {
            if !crate::state::remove_constraint(name) {
                return Err(GleaphError::ExecutionError(format!(
                    "constraint '{name}' does not exist"
                )));
            }
            Ok(Some(MutationOutcome {
                result: MutationResult {
                    affected_vertices: 0,
                    affected_edges: 0,
                    warnings: vec![info_diagnostic(format!("constraint '{name}' dropped"))],
                },
                affected_vertex_ids: Vec::new(),
            }))
        }
        _ => Ok(None),
    }
}

/// Validate that existing graph data satisfies the constraint being created.
fn validate_constraint_against_existing_data(
    label: &str,
    property: &str,
    kind: crate::state::StoredConstraintKind,
) -> Result<(), GleaphError> {
    with_state(|g| {
        let vertices = g.scan_vertices_by_label(label);
        match kind {
            crate::state::StoredConstraintKind::Unique => {
                let mut seen: Vec<(Value, u32)> = Vec::new();
                for vid in vertices.iter() {
                    if let Some(val) = g.get_single_vertex_property(vid, property)
                        && val != Value::Null
                    {
                        if let Some((_, prev_id)) = seen.iter().find(|(v, _)| v == &val) {
                            return Err(GleaphError::ValidationError(format!(
                                "UNIQUE constraint violation: vertex {prev_id} and {vid} \
                                     with label '{label}' both have {property} = {val:?}"
                            )));
                        }
                        seen.push((val, vid));
                    }
                }
            }
            crate::state::StoredConstraintKind::NotNull => {
                for vid in vertices.iter() {
                    let val = g.get_single_vertex_property(vid, property);
                    if val.is_none() || val == Some(Value::Null) {
                        return Err(GleaphError::ValidationError(format!(
                            "NOT NULL constraint violation: vertex {vid} with label '{label}' \
                             is missing property '{property}'"
                        )));
                    }
                }
            }
        }
        Ok(())
    })
}

/// Enforce named constraints on mutation statements.
///
/// Called after graph-type label validation. Checks UNIQUE and NOT NULL constraints
/// against the data that will be modified by the statement.
fn enforce_constraints(stmt: &Statement) -> Result<(), GleaphError> {
    let constraints = crate::state::list_constraints();
    if constraints.is_empty() {
        return Ok(());
    }
    match stmt {
        Statement::Create(cs) => {
            for create in cs {
                match create {
                    CreateStmt::Node(n) => {
                        for c in &constraints {
                            if !n.node.labels.iter().any(|l| l == &c.label) {
                                continue;
                            }
                            enforce_constraint_on_node_create(&n.node.props_hint, c)?;
                        }
                    }
                    CreateStmt::Edge(e) => {
                        for c in &constraints {
                            if e.left.labels.iter().any(|l| l == &c.label) {
                                enforce_constraint_on_node_create(&e.left.props_hint, c)?;
                            }
                            if e.right.labels.iter().any(|l| l == &c.label) {
                                enforce_constraint_on_node_create(&e.right.props_hint, c)?;
                            }
                        }
                    }
                }
            }
        }
        Statement::Merge(MergeStmt { create, .. }) => match create {
            CreateStmt::Node(n) => {
                for c in &constraints {
                    if !n.node.labels.iter().any(|l| l == &c.label) {
                        continue;
                    }
                    enforce_constraint_on_node_create(&n.node.props_hint, c)?;
                }
            }
            CreateStmt::Edge(e) => {
                for c in &constraints {
                    if e.left.labels.iter().any(|l| l == &c.label) {
                        enforce_constraint_on_node_create(&e.left.props_hint, c)?;
                    }
                    if e.right.labels.iter().any(|l| l == &c.label) {
                        enforce_constraint_on_node_create(&e.right.props_hint, c)?;
                    }
                }
            }
        },
        _ => {}
    }
    Ok(())
}

/// Check a node's properties against a constraint on CREATE/MERGE.
fn enforce_constraint_on_node_create(
    props: &[(String, gleaph_gql::ast::Expr)],
    c: &crate::state::StoredConstraint,
) -> Result<(), GleaphError> {
    match c.kind {
        crate::state::StoredConstraintKind::NotNull => {
            let has_prop = props.iter().any(|(k, v)| {
                k == &c.property && !matches!(v, gleaph_gql::ast::Expr::Literal(Value::Null))
            });
            if !has_prop {
                return Err(GleaphError::ValidationError(format!(
                    "NOT NULL constraint '{}': property '{}' is required on :{} nodes",
                    c.name, c.property, c.label
                )));
            }
        }
        crate::state::StoredConstraintKind::Unique => {
            // Find the value being set
            if let Some((_, gleaph_gql::ast::Expr::Literal(val))) =
                props.iter().find(|(k, _)| k == &c.property)
                && *val != Value::Null
            {
                // Scan vertices with the matching label and check property equality.
                let conflict = with_state(|g| {
                    let verts = g.scan_vertices_by_label(&c.label);
                    for vid in verts {
                        if let Some(v) = g.get_single_vertex_property(vid, &c.property)
                            && v == *val
                        {
                            return Some(vid);
                        }
                    }
                    None
                });
                if let Some(vid) = conflict {
                    return Err(GleaphError::ValidationError(format!(
                        "UNIQUE constraint '{}': vertex {} with label '{}' already has {} = {:?}",
                        c.name, vid, c.label, c.property, val
                    )));
                }
            }
        }
    }
    Ok(())
}

/// §12: Validates that labels used in a mutation statement are allowed by the active graph type.
///
/// If no graph type is active, all labels are allowed (open schema).
fn enforce_graph_type_labels(stmt: &Statement) -> Result<(), GleaphError> {
    let gt = match crate::state::get_active_graph_type() {
        Some(gt) => gt,
        None => return Ok(()), // open schema
    };

    match stmt {
        Statement::Create(cs) => {
            for create in cs {
                match create {
                    CreateStmt::Node(n) => {
                        check_node_labels(&n.node.labels, &gt.node_labels)?;
                        for nt in find_matching_node_types(&n.node.labels, &gt) {
                            validate_properties_against_type(
                                &n.node.props_hint,
                                &nt.properties,
                                true,
                            )?;
                        }
                    }
                    CreateStmt::Edge(e) => {
                        check_node_labels(&e.left.labels, &gt.node_labels)?;
                        check_node_labels(&e.right.labels, &gt.node_labels)?;
                        if let Some(ref label) = e.edge.label {
                            check_edge_label(label, &gt.edge_labels)?;
                            enforce_edge_type_endpoints(
                                &gt,
                                label,
                                &e.left.labels,
                                &e.right.labels,
                            )?;
                            validate_edge_properties_against_types(&gt, label, &e.edge.properties)?;
                        }
                        for nt in find_matching_node_types(&e.left.labels, &gt) {
                            validate_properties_against_type(
                                &e.left.props_hint,
                                &nt.properties,
                                true,
                            )?;
                        }
                        for nt in find_matching_node_types(&e.right.labels, &gt) {
                            validate_properties_against_type(
                                &e.right.props_hint,
                                &nt.properties,
                                true,
                            )?;
                        }
                    }
                }
            }
        }
        Statement::Merge(MergeStmt { create, .. }) => match create {
            CreateStmt::Node(n) => {
                check_node_labels(&n.node.labels, &gt.node_labels)?;
                for nt in find_matching_node_types(&n.node.labels, &gt) {
                    validate_properties_against_type(&n.node.props_hint, &nt.properties, true)?;
                }
            }
            CreateStmt::Edge(e) => {
                check_node_labels(&e.left.labels, &gt.node_labels)?;
                check_node_labels(&e.right.labels, &gt.node_labels)?;
                if let Some(ref label) = e.edge.label {
                    check_edge_label(label, &gt.edge_labels)?;
                    enforce_edge_type_endpoints(&gt, label, &e.left.labels, &e.right.labels)?;
                    validate_edge_properties_against_types(&gt, label, &e.edge.properties)?;
                }
            }
        },
        Statement::Set(s) => {
            for item in &s.set_clause.items {
                if let SetItem::Label { label, .. } = item {
                    check_node_labels(std::slice::from_ref(label), &gt.node_labels)?;
                }
                // Property type check: if a SET property matches a node type def, validate type
                if let SetItem::Property {
                    property, value, ..
                } = item
                {
                    for nt in &gt.node_types {
                        if let Some(def) = nt.properties.iter().find(|d| d.name == *property)
                            && let gleaph_gql::ast::Expr::Literal(val) = value
                            && !matches_stored_value_type(val, &def.value_type)
                        {
                            return Err(GleaphError::ValidationError(format!(
                                "property '{}' has type {} but expected {:?}",
                                property,
                                value_type_name(val),
                                def.value_type
                            )));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Validates that edge endpoint labels satisfy at least one matching edge type definition.
///
/// If no edge types are defined for the given label, the edge is allowed (open schema).
/// Each `from_types`/`to_types` entry is resolved: if it matches a node type name, expand to that
/// type's labels; otherwise treat as a raw label.
fn enforce_edge_type_endpoints(
    gt: &crate::state::StoredGraphType,
    edge_label: &str,
    src_labels: &[String],
    dst_labels: &[String],
) -> Result<(), GleaphError> {
    let matching: Vec<_> = gt
        .edge_types
        .iter()
        .filter(|et| et.label == edge_label)
        .collect();
    if matching.is_empty() {
        return Ok(()); // open schema — no edge type defined for this label
    }
    // At least one edge type must be satisfied
    let satisfied = matching.iter().any(|et| {
        let from_ok = endpoints_match(&et.from_types, src_labels, &gt.node_types);
        let to_ok = endpoints_match(&et.to_types, dst_labels, &gt.node_types);
        from_ok && to_ok
    });
    if !satisfied {
        return Err(GleaphError::ValidationError(format!(
            "edge label '{edge_label}' does not allow endpoint combination \
             ({src_labels:?})->({dst_labels:?}); defined edge types: {}",
            matching
                .iter()
                .map(|et| format!(
                    "{}:({:?})-[:{}]->({:?})",
                    et.name, et.from_types, et.label, et.to_types
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// Checks whether `actual_labels` satisfies the endpoint constraint `type_names`.
///
/// Each entry in `type_names` is either a node type name (resolved to its labels) or a raw label.
/// At least one entry must be satisfied: all labels of the resolved type must appear in `actual_labels`.
fn endpoints_match(
    type_names: &[String],
    actual_labels: &[String],
    node_types: &[crate::state::StoredNodeType],
) -> bool {
    if type_names.is_empty() {
        return true;
    }
    type_names.iter().any(|tn| {
        let resolved = resolve_type_to_labels(tn, node_types);
        resolved
            .iter()
            .all(|l| actual_labels.iter().any(|a| a == l))
    })
}

/// Resolves a type name to a list of labels. If the name matches a node type, returns that type's
/// labels; otherwise returns the name itself as a single-element list (raw label fallback).
fn resolve_type_to_labels<'a>(
    type_name: &'a str,
    node_types: &'a [crate::state::StoredNodeType],
) -> Vec<&'a str> {
    if let Some(nt) = node_types
        .iter()
        .find(|nt| nt.name.eq_ignore_ascii_case(type_name))
    {
        nt.labels.iter().map(|s| s.as_str()).collect()
    } else {
        vec![type_name]
    }
}

/// Finds matching edge types for a given edge label and validates properties against them.
fn validate_edge_properties_against_types(
    gt: &crate::state::StoredGraphType,
    edge_label: &str,
    props: &[(String, gleaph_gql::ast::Expr)],
) -> Result<(), GleaphError> {
    for et in gt.edge_types.iter().filter(|et| et.label == edge_label) {
        if !et.properties.is_empty() {
            validate_properties_against_type(props, &et.properties, true)?;
        }
    }
    Ok(())
}

fn check_node_labels(labels: &[String], allowed: &[String]) -> Result<(), GleaphError> {
    for label in labels {
        if !allowed.contains(label) {
            return Err(GleaphError::ValidationError(format!(
                "label '{label}' is not allowed by the active graph type (allowed node labels: {allowed:?})"
            )));
        }
    }
    Ok(())
}

fn check_edge_label(label: &str, allowed: &[String]) -> Result<(), GleaphError> {
    if !allowed.iter().any(|a| a == label) {
        return Err(GleaphError::ValidationError(format!(
            "edge label '{label}' is not allowed by the active graph type (allowed edge labels: {allowed:?})"
        )));
    }
    Ok(())
}

/// Returns a human-readable type name for a `Value`.
fn value_type_name(val: &Value) -> &'static str {
    match val {
        Value::Int8(_) => "INT8",
        Value::Int16(_) => "INT16",
        Value::Int32(_) => "INT32",
        Value::Int64(_) => "INT64",
        Value::Int128(_) => "INT128",
        Value::Int256(_) => "INT256",
        Value::Uint8(_) => "UINT8",
        Value::Uint16(_) => "UINT16",
        Value::Uint32(_) => "UINT32",
        Value::Uint64(_) => "UINT64",
        Value::Uint128(_) => "UINT128",
        Value::Uint256(_) => "UINT256",
        Value::Float32(_) => "FLOAT32",
        Value::Float64(_) => "FLOAT64",
        Value::Text(_) => "TEXT",
        Value::Bool(_) => "BOOL",
        Value::Null => "NULL",
        Value::Timestamp(_) => "TIMESTAMP",
        Value::List(_) => "LIST",
        Value::Bytes(_) => "BYTES",
        Value::Date(_) => "DATE",
        Value::Time(_) => "TIME",
        Value::DateTime(_, _) => "DATETIME",
        Value::Duration(_, _) => "DURATION",
        Value::Path(_) => "PATH",
        Value::Principal(_) => "PRINCIPAL",
        Value::Decimal(_) => "DECIMAL",
    }
}

/// Converts an AST `ValueType` to a `StoredValueType`.
fn ast_scalar_to_stored(s: gleaph_gql::ast::ScalarType) -> crate::state::StoredScalarType {
    use crate::state::StoredScalarType as S;
    use gleaph_gql::ast::ScalarType as A;
    match s {
        A::Int64 => S::Int64,
        A::Float64 => S::Float64,
        A::Float32 => S::Float32,
        A::Text => S::Text,
        A::Bool => S::Bool,
        A::Timestamp => S::Timestamp,
        A::Bytes => S::Bytes,
        A::Date => S::Date,
        A::Time => S::Time,
        A::DateTime => S::DateTime,
        A::Duration => S::Duration,
        A::Principal => S::Principal,
        A::Decimal => S::Decimal,
        A::Uint64 => S::Uint64,
        A::Int8 => S::Int8,
        A::Int16 => S::Int16,
        A::Int32 => S::Int32,
        A::Int128 => S::Int128,
        A::Int256 => S::Int256,
        A::Uint8 => S::Uint8,
        A::Uint16 => S::Uint16,
        A::Uint32 => S::Uint32,
        A::Uint128 => S::Uint128,
        A::Uint256 => S::Uint256,
    }
}

fn ast_value_type_to_stored(vt: gleaph_gql::ast::ValueType) -> crate::state::StoredValueType {
    use crate::state::StoredValueType;
    use gleaph_gql::ast::ValueType;
    match vt {
        ValueType::Int64 => StoredValueType::Int64,
        ValueType::Float64 => StoredValueType::Float64,
        ValueType::Float32 => StoredValueType::Float32,
        ValueType::Text => StoredValueType::Text,
        ValueType::Bool => StoredValueType::Bool,
        ValueType::Timestamp => StoredValueType::Timestamp,
        ValueType::List => StoredValueType::List,
        ValueType::TypedList(s) => StoredValueType::TypedList(ast_scalar_to_stored(s)),
        ValueType::Bytes => StoredValueType::Bytes,
        ValueType::Date => StoredValueType::Date,
        ValueType::Time => StoredValueType::Time,
        ValueType::DateTime => StoredValueType::DateTime,
        ValueType::Duration => StoredValueType::Duration,
        ValueType::Decimal => StoredValueType::Decimal,
        ValueType::Uint64 => StoredValueType::Uint64,
        ValueType::Int8 => StoredValueType::Int8,
        ValueType::Int16 => StoredValueType::Int16,
        ValueType::Int32 => StoredValueType::Int32,
        ValueType::Int128 => StoredValueType::Int128,
        ValueType::Int256 => StoredValueType::Int256,
        ValueType::Uint8 => StoredValueType::Uint8,
        ValueType::Uint16 => StoredValueType::Uint16,
        ValueType::Uint32 => StoredValueType::Uint32,
        ValueType::Uint128 => StoredValueType::Uint128,
        ValueType::Uint256 => StoredValueType::Uint256,
        ValueType::Null => StoredValueType::Text, // fallback — Null not expected in property defs
        ValueType::TextConstrained {
            min_length,
            max_length,
            fixed,
        } => StoredValueType::TextConstrained {
            min_length,
            max_length,
            fixed,
        },
        ValueType::BytesConstrained {
            min_length,
            max_length,
            fixed,
        } => StoredValueType::BytesConstrained {
            min_length,
            max_length,
            fixed,
        },
    }
}

fn stored_scalar_type_name(s: crate::state::StoredScalarType) -> &'static str {
    use crate::state::StoredScalarType;
    match s {
        StoredScalarType::Int64 => "INT64",
        StoredScalarType::Float64 => "FLOAT64",
        StoredScalarType::Float32 => "FLOAT32",
        StoredScalarType::Text => "TEXT",
        StoredScalarType::Bool => "BOOL",
        StoredScalarType::Timestamp => "TIMESTAMP",
        StoredScalarType::Bytes => "BYTES",
        StoredScalarType::Date => "DATE",
        StoredScalarType::Time => "TIME",
        StoredScalarType::DateTime => "DATETIME",
        StoredScalarType::Duration => "DURATION",
        StoredScalarType::Principal => "PRINCIPAL",
        StoredScalarType::Decimal => "DECIMAL",
        StoredScalarType::Uint64 => "UINT64",
        StoredScalarType::Int8 => "INT8",
        StoredScalarType::Int16 => "INT16",
        StoredScalarType::Int32 => "INT32",
        StoredScalarType::Int128 => "INT128",
        StoredScalarType::Int256 => "INT256",
        StoredScalarType::Uint8 => "UINT8",
        StoredScalarType::Uint16 => "UINT16",
        StoredScalarType::Uint32 => "UINT32",
        StoredScalarType::Uint128 => "UINT128",
        StoredScalarType::Uint256 => "UINT256",
    }
}

/// Checks whether a `Value` matches the expected `StoredValueType`.
fn matches_stored_value_type(val: &Value, ty: &crate::state::StoredValueType) -> bool {
    use crate::state::StoredValueType;
    match (val, ty) {
        (
            v,
            StoredValueType::Int64
            | StoredValueType::Int8
            | StoredValueType::Int16
            | StoredValueType::Int32
            | StoredValueType::Int128
            | StoredValueType::Int256,
        ) if v.is_signed_int() => true,
        (
            v,
            StoredValueType::Uint64
            | StoredValueType::Uint8
            | StoredValueType::Uint16
            | StoredValueType::Uint32
            | StoredValueType::Uint128
            | StoredValueType::Uint256,
        ) if v.is_unsigned_int() => true,
        (Value::Float64(_), StoredValueType::Float64) => true,
        (Value::Float32(_), StoredValueType::Float32) => true,
        (Value::Text(_), StoredValueType::Text) => true,
        (Value::Bool(_), StoredValueType::Bool) => true,
        (Value::Timestamp(_), StoredValueType::Timestamp) => true,
        (Value::List(_), StoredValueType::List)
        | (Value::List(_), StoredValueType::TypedList(_)) => true,
        (Value::Bytes(_), StoredValueType::Bytes) => true,
        (Value::Date(_), StoredValueType::Date) => true,
        (Value::Time(_), StoredValueType::Time) => true,
        (Value::DateTime(_, _), StoredValueType::DateTime) => true,
        (Value::Duration(_, _), StoredValueType::Duration) => true,
        (Value::Decimal(_), StoredValueType::Decimal) => true,
        (
            Value::Text(s),
            StoredValueType::TextConstrained {
                min_length,
                max_length,
                fixed,
            },
        ) => {
            let char_len = s.chars().count() as u32;
            if *fixed {
                char_len <= *max_length
            } else {
                char_len >= *min_length && char_len <= *max_length
            }
        }
        (
            Value::Bytes(b),
            StoredValueType::BytesConstrained {
                min_length,
                max_length,
                fixed,
            },
        ) => {
            let len = b.len() as u32;
            if *fixed {
                len <= *max_length
            } else {
                len >= *min_length && len <= *max_length
            }
        }
        (Value::Null, _) => true, // NULL always passes (required check is separate)
        _ => false,
    }
}

/// Finds stored node types whose labels are a subset of the given labels.
fn find_matching_node_types<'a>(
    labels: &[String],
    gt: &'a crate::state::StoredGraphType,
) -> Vec<&'a crate::state::StoredNodeType> {
    gt.node_types
        .iter()
        .filter(|nt| !nt.properties.is_empty() && nt.labels.iter().all(|l| labels.contains(l)))
        .collect()
}

/// Validates property hints against stored property definitions (open schema).
///
/// - Required properties must be present (unless `check_required` is false for SET).
/// - Literal values must match the declared type.
/// - Properties not in the schema are allowed (open schema).
fn validate_properties_against_type(
    props: &[(String, gleaph_gql::ast::Expr)],
    defs: &[crate::state::StoredPropertyDef],
    check_required: bool,
) -> Result<(), GleaphError> {
    use gleaph_gql::ast::Expr;
    // Check required properties are present
    if check_required {
        for def in defs {
            if def.required && !props.iter().any(|(name, _)| name == &def.name) {
                return Err(GleaphError::ValidationError(format!(
                    "required property '{}' is missing",
                    def.name
                )));
            }
        }
    }
    // Check type compatibility for literal values
    for (name, expr) in props {
        if let Some(def) = defs.iter().find(|d| d.name == *name)
            && let Expr::Literal(val) = expr
            && !matches_stored_value_type(val, &def.value_type)
        {
            return Err(GleaphError::ValidationError(format!(
                "property '{}' has type {} but expected {:?}",
                name,
                value_type_name(val),
                def.value_type
            )));
        }
    }
    Ok(())
}

pub fn enforce_limits(gql: &str) -> Result<(), GleaphError> {
    if gql.len() > MAX_QUERY_LEN_HARD {
        return Err(GleaphError::UnsupportedFeature(format!(
            "query too long (max {} bytes)",
            MAX_QUERY_LEN_HARD
        )));
    }
    Ok(())
}

fn enforce_quota_for_mutation(stmt: &Statement) -> Result<(), GleaphError> {
    let quota = crate::state::get_quota();
    if quota.max_vertices == 0 && quota.max_edges == 0 {
        return Ok(());
    }
    // Only check write statements that can add vertices or edges.
    if !matches!(stmt, Statement::Create(_) | Statement::Merge(_)) {
        return Ok(());
    }
    // `vertex_count()` returns the PMA vertex-array size which starts at the pre-allocated
    // capacity (`initial_vertex_capacity`).  Subtract that to get the count of GQL-created vertices.
    let initial_capacity = u64::from(crate::state::config_initial_vertex_capacity());
    let (gql_vertices, gql_edges) = with_state(|g| {
        (
            g.vertex_count().saturating_sub(initial_capacity),
            g.edge_count(),
        )
    });
    if quota.max_vertices > 0 && gql_vertices >= quota.max_vertices {
        return Err(GleaphError::UnsupportedFeature(format!(
            "vertex quota exceeded (current: {}, max: {})",
            gql_vertices, quota.max_vertices
        )));
    }
    if quota.max_edges > 0 && gql_edges >= quota.max_edges {
        return Err(GleaphError::UnsupportedFeature(format!(
            "edge quota exceeded (current: {}, max: {})",
            gql_edges, quota.max_edges
        )));
    }
    Ok(())
}

#[allow(dead_code)]
fn enforce_result_limits(result: &QueryResult) -> Result<(), GleaphError> {
    if result.rows.len() > HARD_MAX_ROWS {
        return Err(GleaphError::ExecutionError(format!(
            "result row count {} exceeds hard cap {}",
            result.rows.len(),
            HARD_MAX_ROWS
        )));
    }
    if result.rows.len() > DEFAULT_MAX_ROWS {
        return Err(GleaphError::ExecutionError(format!(
            "result row count {} exceeds default cap {}",
            result.rows.len(),
            DEFAULT_MAX_ROWS
        )));
    }
    if result.stats.execution_steps > HARD_MAX_EXECUTION_STEPS {
        return Err(GleaphError::ExecutionError(format!(
            "execution steps {} exceed hard cap {}",
            result.stats.execution_steps, HARD_MAX_EXECUTION_STEPS
        )));
    }
    Ok(())
}

// ── P5 Prepared statements ────────────────────────────────────────────────

/// Prepare a GQL statement for repeated execution.
/// Extract column names from a query's RETURN clause.
fn extract_columns(stmt: &Statement) -> Vec<String> {
    let rc = match stmt {
        Statement::Query(q) => &q.return_clause,
        _ => return vec![],
    };
    if rc.star {
        vec!["*".into()]
    } else if rc.no_bindings || rc.finish {
        vec![]
    } else {
        rc.items
            .iter()
            .map(|item| gleaph_gql::executor::column_name_for_return_item(item))
            .collect()
    }
}

/// Check whether a GQL source uses the `caller()` function.
///
/// Uses the existing `collect_function_calls_from_stmt` AST walker to reliably
/// detect `caller()` calls regardless of nesting depth.
fn stmt_uses_caller(stmt: &Statement) -> bool {
    let mut calls = std::collections::BTreeSet::new();
    gleaph_gql::executor::collect_function_calls_from_stmt(stmt, &mut calls);
    calls.iter().any(|name| name.eq_ignore_ascii_case("caller"))
}

fn current_table_stats() -> Result<TableStats, GleaphError> {
    with_state(|g| {
        let vertex_count = g.vertex_count();
        let edge_count = g.edge_count();
        let mut stats = TableStats {
            vertex_count,
            edge_count,
            avg_degree: if vertex_count == 0 {
                1.0
            } else {
                (edge_count as f64 / vertex_count as f64).max(1.0)
            },
            label_cardinality: g.label_cardinalities(),
            ..TableStats::default()
        };
        for (key, &sel) in g.get_property_selectivity() {
            stats.property_selectivity.insert(key.clone(), sel);
        }
        for idx in g.list_property_indexes() {
            if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality {
                stats
                    .indexed_vertex_properties
                    .insert(idx.property_name.clone());
                stats
                    .property_selectivity
                    .entry(format!("vertex:{}", idx.property_name))
                    .or_insert(0.1);
            }
            if idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Range {
                stats
                    .range_indexed_vertex_properties
                    .insert(idx.property_name.clone());
            }
            if idx.entity_type == EntityType::Edge && idx.index_type == IndexType::Equality {
                stats
                    .indexed_edge_properties
                    .insert(idx.property_name.clone());
                stats
                    .property_selectivity
                    .entry(format!("edge:{}", idx.property_name))
                    .or_insert(0.1);
            }
        }
        Ok(stats)
    })
}

fn build_prepared_query_plan(stmt: &Statement) -> gleaph_gql::plan::PhysicalPlan {
    sync_node_type_defs();
    match current_table_stats().and_then(|stats| build_runtime_plan_with_stats(stmt, Some(&stats))) {
        Ok(plan) => plan,
        Err(_) => gleaph_gql::plan::PhysicalPlan {
            ops: vec![],
            annotations: Default::default(),
            query: None,
        },
    }
}

fn parse_sort_expr(expr_source: &str) -> Result<Expr, GleaphError> {
    let stmt = parse_statement(&format!("RETURN {expr_source}"))?;
    let Statement::Query(query) = stmt else {
        return Err(GleaphError::ValidationError(
            "failed to parse dynamic sort expression".into(),
        ));
    };
    query
        .return_clause
        .items
        .into_iter()
        .next()
        .map(|item| item.expr)
        .ok_or_else(|| {
            GleaphError::ValidationError("failed to extract dynamic sort expression".into())
        })
}

fn validate_prepared_sort_expr(query: &QueryStmt, expr: Expr) -> Result<Expr, GleaphError> {
    let mut query_with_sort = query.clone();
    query_with_sort.order_by = Some(gleaph_gql::ast::OrderBy {
        items: vec![gleaph_gql::ast::OrderByItem {
            expr: expr.clone(),
            descending: false,
            nulls_first: None,
        }],
    });
    validate_statement(&Statement::Query(query_with_sort))?;
    Ok(expr)
}

fn build_allowed_sorts(
    stmt: &Statement,
    options: &gleaph_types::PreparedOptions,
) -> Result<Vec<crate::state::PreparedSortDef>, GleaphError> {
    if options.allowed_sorts.is_empty() {
        return Ok(vec![]);
    }
    let query = match stmt {
        Statement::Query(query) => query,
        _ => {
            return Err(GleaphError::ValidationError(
                "dynamic sort options are only supported for query prepared statements".into(),
            ));
        }
    };
    if query.order_by.is_some() {
        return Err(GleaphError::ValidationError(
            "prepared statement cannot define both ORDER BY in GQL and dynamic sort options".into(),
        ));
    }
    let mut out = Vec::with_capacity(options.allowed_sorts.len());
    let mut seen = std::collections::BTreeSet::new();
    for sort in &options.allowed_sorts {
        if !seen.insert(sort.key.to_ascii_lowercase()) {
            return Err(GleaphError::ValidationError(format!(
                "duplicate prepared sort key '{}'",
                sort.key
            )));
        }
        let expr = validate_prepared_sort_expr(query, parse_sort_expr(&sort.expr)?)?;
        out.push(crate::state::PreparedSortDef {
            key: sort.key.clone(),
            expr_source: sort.expr.clone(),
            expr,
        });
    }
    if let Some(default_sort) = &options.default_sort {
        for spec in default_sort {
            if !out.iter().any(|sort| sort.key == spec.key) {
                return Err(GleaphError::ValidationError(format!(
                    "default_sort key '{}' is not present in allowed_sorts",
                    spec.key
                )));
            }
        }
    }
    Ok(out)
}

fn materialize_prepared_order_by(
    ps: &crate::state::PreparedStatement,
    requested: Option<Vec<gleaph_types::PreparedSortSpec>>,
) -> Result<Option<gleaph_gql::ast::OrderBy>, GleaphError> {
    let requested = match requested {
        Some(specs) => Some(specs),
        None => ps.default_sort.clone(),
    };
    let Some(specs) = requested else {
        return match &ps.stmt {
            Statement::Query(query) => Ok(query.order_by.clone()),
            _ => Ok(None),
        };
    };
    if ps.allowed_sorts.is_empty() {
        return Err(GleaphError::ValidationError(
            "dynamic sort is not enabled for this prepared statement".into(),
        ));
    }
    let mut items = Vec::with_capacity(specs.len());
    for spec in specs {
        let def = ps
            .allowed_sorts
            .iter()
            .find(|sort| sort.key == spec.key)
            .ok_or_else(|| {
                GleaphError::ValidationError(format!("unknown prepared sort key '{}'", spec.key))
            })?;
        items.push(gleaph_gql::ast::OrderByItem {
            expr: def.expr.clone(),
            descending: spec.descending,
            nulls_first: spec.nulls_first,
        });
    }
    Ok(Some(gleaph_gql::ast::OrderBy { items }))
}

pub fn prepare_statement(
    name: &str,
    gql: &str,
    options: Option<gleaph_types::PreparedOptions>,
) -> Result<gleaph_types::PreparedStatementInfo, GleaphError> {
    enforce_limits(gql)?;
    let stmt = parse_statement(gql)?;
    validate_statement(&stmt)?;
    let is_mutation = matches!(
        stmt,
        Statement::Create(_)
            | Statement::Delete(_)
            | Statement::Set(_)
            | Statement::Remove(_)
            | Statement::Merge(_)
    );
    let schema = ActiveGraphTypeSchema::from_active();
    let inferred_params = gleaph_gql::param_inference::infer_parameter_types(&stmt, &schema);
    // Phase 3: extract parameter inference conflict diagnostics.
    let param_conflict_diagnostics =
        gleaph_gql::param_inference::conflict_diagnostics(&inferred_params);
    // Strict mode: reject prepare if any parameter inference conflicts exist.
    if crate::state::is_strict_type_check() {
        if let Some(first) = param_conflict_diagnostics.first() {
            return Err(GleaphError::ValidationError(format!(
                "strict type check: {}",
                first.message
            )));
        }
    }
    let parameters: Vec<gleaph_types::PreparedParameterInfo> = inferred_params
        .into_iter()
        .map(|(name, ip)| gleaph_gql::param_inference::to_prepared_param_info(name, &ip))
        .collect();
    let options = options.unwrap_or_default();
    let allowed_sorts = build_allowed_sorts(&stmt, &options)?;

    let columns = extract_columns(&stmt);
    let requires_caller = stmt_uses_caller(&stmt);
    let static_type_warnings =
        gleaph_gql::type_check::type_check_statement_with_schema(&stmt, &schema);
    if !is_mutation && has_impossible_pattern_warning(&static_type_warnings) {
        let message = static_type_warnings
            .iter()
            .find(|warning| warning.kind == gleaph_gql::type_check::WarningKind::ImpossiblePattern)
            .map(|warning| warning.message.clone())
            .unwrap_or_else(|| "prepared query has a statically impossible pattern".into());
        return Err(GleaphError::ValidationError(format!(
            "cannot prepare query: {message}"
        )));
    }
    let mut type_warning_messages = structured_type_diagnostics(static_type_warnings);
    // Merge parameter inference conflict diagnostics into type warnings.
    type_warning_messages.extend(param_conflict_diagnostics);

    let plan = if !is_mutation {
        build_prepared_query_plan(&stmt)
    } else {
        if !allowed_sorts.is_empty() || options.default_sort.is_some() {
            return Err(GleaphError::ValidationError(
                "dynamic sort options are only supported for query prepared statements".into(),
            ));
        }
        gleaph_gql::plan::PhysicalPlan {
            ops: vec![],
            annotations: Default::default(),
            query: None,
        }
    };

    let info = gleaph_types::PreparedStatementInfo {
        name: name.to_string(),
        kind: if is_mutation {
            gleaph_types::PreparedKind::Mutation
        } else {
            gleaph_types::PreparedKind::Query
        },
        parameters: parameters.clone(),
        columns,
        requires_caller,
        source: gql.to_string(),
        description: options.description.clone(),
        allowed_sorts: options.allowed_sorts.clone(),
        default_sort: options.default_sort.clone(),
        type_warnings: type_warning_messages.clone(),
    };

    crate::state::prepare_stmt(
        name.to_string(),
        crate::state::PreparedStatement {
            source: gql.to_string(),
            description: options.description,
            plan,
            parameters,
            is_mutation,
            stmt,
            allowed_sorts,
            default_sort: options.default_sort,
            type_warnings: type_warning_messages,
        },
    )?;

    Ok(info)
}

/// Execute a previously prepared read query.
pub fn execute_prepared_query(
    name: &str,
    params: &std::collections::HashMap<String, Value>,
    sort: Option<Vec<gleaph_types::PreparedSortSpec>>,
) -> Result<QueryResult, GleaphError> {
    let ps = crate::state::get_prepared_stmt(name)
        .ok_or_else(|| GleaphError::ExecutionError(format!("no prepared statement '{name}'")))?;
    if ps.is_mutation {
        return Err(GleaphError::ExecutionError(
            "use execute_prepared_mutation for mutations".into(),
        ));
    }
    // Check all required params are present.
    for parameter in &ps.parameters {
        if parameter.required && !params.contains_key(parameter.name.as_str()) {
            return Err(GleaphError::ValidationError(format!(
                "undefined parameter '${}'",
                parameter.name
            )));
        }
    }
    sync_node_type_defs();
    set_current_time(ic_timestamp());
    let _caller_guard = inject_caller();
    struct ClearTimeGuardPrepared;
    impl Drop for ClearTimeGuardPrepared {
        fn drop(&mut self) {
            clear_current_time();
        }
    }
    let _clear_time = ClearTimeGuardPrepared;
    let default_steps = if statement_has_aggregation(&ps.stmt) {
        HARD_MAX_EXECUTION_STEPS_HEAVY_AGG_QUERY
    } else {
        HARD_MAX_EXECUTION_STEPS
    };
    let stmt_with_sort = if ps.allowed_sorts.is_empty() && sort.is_none() {
        None
    } else {
        let mut stmt = ps.stmt.clone();
        let Statement::Query(query) = &mut stmt else {
            return Err(GleaphError::ValidationError(
                "dynamic sort options are only supported for query prepared statements".into(),
            ));
        };
        query.order_by = materialize_prepared_order_by(&ps, sort)?;
        Some(stmt)
    };
    // If plan has a query, use the cached plan; otherwise fall back to statement execution
    // (compound/bare-RETURN queries bypass the planner).
    if stmt_with_sort.is_none() && ps.plan.query.is_some() {
        use gleaph_gql::executor::{clear_query_params, set_query_params};
        set_query_params(params.clone());
        let hasher = RapidRandomState::new();
        let result = with_state(|g| {
            execute_plan_with_params_and_hasher(
                &ps.plan,
                g,
                params,
                ExecutionLimits {
                    max_rows: Some(HARD_MAX_ROWS),
                    max_execution_steps: Some(default_steps),
                },
                &hasher,
            )
        });
        clear_query_params();
        result.map(|mut result| {
            result.warnings = ps.type_warnings.clone();
            result
        })
    } else {
        use gleaph_gql::executor::{clear_query_params, set_query_params};
        set_query_params(params.clone());
        let stmt = stmt_with_sort.as_ref().unwrap_or(&ps.stmt);
        let result = with_state(|g| {
            if stmt_with_sort.is_some() {
                let plan = current_table_stats()
                    .and_then(|stats| build_plan_with_stats(stmt, Some(&stats)))
                    .unwrap_or_else(|_| gleaph_gql::plan::PhysicalPlan {
                        ops: vec![],
                        annotations: Default::default(),
                        query: None,
                    });
                if plan.query.is_some() {
                    let hasher = RapidRandomState::new();
                    execute_plan_with_params_and_hasher(
                        &plan,
                        g,
                        params,
                        ExecutionLimits {
                            max_rows: Some(HARD_MAX_ROWS),
                            max_execution_steps: Some(default_steps),
                        },
                        &hasher,
                    )
                } else {
                    execute_query_statement_with_limits(
                        stmt,
                        g,
                        ExecutionLimits {
                            max_rows: Some(HARD_MAX_ROWS),
                            max_execution_steps: Some(default_steps),
                        },
                    )
                }
            } else {
                execute_query_statement_with_limits(
                    stmt,
                    g,
                    ExecutionLimits {
                        max_rows: Some(HARD_MAX_ROWS),
                        max_execution_steps: Some(default_steps),
                    },
                )
            }
        });
        clear_query_params();
        result.map(|mut result| {
            result.warnings = ps.type_warnings.clone();
            result
        })
    }
}

/// Execute a previously prepared mutation.
pub fn execute_prepared_mutation(
    name: &str,
    params: &std::collections::HashMap<String, Value>,
) -> Result<MutationOutcome, GleaphError> {
    let ps = crate::state::get_prepared_stmt(name)
        .ok_or_else(|| GleaphError::ExecutionError(format!("no prepared statement '{name}'")))?;
    if !ps.is_mutation {
        return Err(GleaphError::ExecutionError(
            "use execute_prepared for read queries".into(),
        ));
    }
    // Check all required params are present.
    for parameter in &ps.parameters {
        if parameter.required && !params.contains_key(parameter.name.as_str()) {
            return Err(GleaphError::ValidationError(format!(
                "undefined parameter '${}'",
                parameter.name
            )));
        }
    }
    enforce_quota_for_mutation(&ps.stmt)?;
    enforce_graph_type_labels(&ps.stmt)?;
    enforce_constraints(&ps.stmt)?;
    sync_node_type_defs();
    set_current_time(ic_timestamp());
    let _caller_guard = inject_caller();
    struct ClearTimeGuardPreparedMut;
    impl Drop for ClearTimeGuardPreparedMut {
        fn drop(&mut self) {
            clear_current_time();
        }
    }
    let _clear_time = ClearTimeGuardPreparedMut;
    // Set query params for the executor.
    use gleaph_gql::executor::{clear_query_params, set_query_params};
    set_query_params(params.clone());
    let ts = ic_timestamp();
    let outcome = with_state_mut(|g| {
        let max_execution_steps = match &ps.stmt {
            Statement::Set(_) | Statement::Remove(_) => HARD_MAX_EXECUTION_STEPS_HEAVY_MUTATION,
            _ => HARD_MAX_EXECUTION_STEPS,
        };
        let outcome = execute_mutation_tracked(
            &ps.stmt,
            g,
            ExecutionLimits {
                max_rows: Some(DEFAULT_MAX_ROWS),
                max_execution_steps: Some(max_execution_steps),
            },
            ts,
        );
        g.refresh_selectivity_if_stale();
        outcome
    });
    clear_query_params();
    outcome
}

/// Drop a prepared statement by name. Returns true if it existed.
pub fn drop_prepared(name: &str) -> bool {
    crate::state::drop_prepared_stmt(name)
}

/// List all prepared statements with metadata.
pub fn list_prepared() -> Vec<gleaph_types::PreparedStatementInfo> {
    crate::state::list_prepared_stmts()
        .into_iter()
        .map(|(name, ps)| gleaph_types::PreparedStatementInfo {
            kind: if ps.is_mutation {
                gleaph_types::PreparedKind::Mutation
            } else {
                gleaph_types::PreparedKind::Query
            },
            parameters: ps.parameters.clone(),
            columns: extract_columns(&ps.stmt),
            requires_caller: stmt_uses_caller(&ps.stmt),
            source: ps.source,
            description: ps.description,
            allowed_sorts: ps
                .allowed_sorts
                .iter()
                .map(|sort| gleaph_types::PreparedSortKey {
                    key: sort.key.clone(),
                    expr: sort.expr_source.clone(),
                })
                .collect(),
            default_sort: ps.default_sort.clone(),
            name,
            type_warnings: ps.type_warnings.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{STATE, init_state};

    fn clear_state_for_test() {
        STATE.with(|s| *s.borrow_mut() = None);
    }

    #[test]
    fn batch_mutate_rejects_overlong_query_before_state_access() {
        clear_state_for_test();
        let gqls = vec!["M".repeat(16 * 1024 + 1)];
        let results = batch_mutate(&gqls);
        assert_eq!(results.len(), 1);
        let err = results
            .into_iter()
            .next()
            .unwrap()
            .expect_err("should fail");
        assert!(matches!(err, GleaphError::UnsupportedFeature(_)));
        assert!(err.to_string().contains("query too long"));
    }

    #[test]
    fn batch_mutate_rejects_parse_error_before_state_access() {
        clear_state_for_test();
        let gqls = vec!["MATCH (a)-[:X]->(b) RETURN a LIMIT 5000000000".to_string()];
        let results = batch_mutate(&gqls);
        assert_eq!(results.len(), 1);
        let err = results
            .into_iter()
            .next()
            .unwrap()
            .expect_err("should fail");
        assert!(matches!(err, GleaphError::ParseError(_)));
        assert!(err.to_string().contains("LIMIT exceeds"));
    }

    #[test]
    fn batch_mutate_fails_all_items_when_any_input_is_invalid() {
        clear_state_for_test();
        let gqls = vec![
            r#"INSERT (:User {name: 'A'})"#.to_string(),
            "MATCH (a)-[:X]->(b) RETURN a LIMIT 5000000000".to_string(),
        ];
        let results = batch_mutate(&gqls);
        assert_eq!(results.len(), 2);
        assert!(
            results
                .iter()
                .all(|r| matches!(r, Err(GleaphError::ParseError(_))))
        );
    }

    #[test]
    fn batch_mutate_returns_per_item_execution_results_after_validation() {
        init_state(8, 0).expect("init state");
        let gqls = vec![
            r#"INSERT (:User {name: 'A'})"#.to_string(),
            "MATCH (a)-[:X]->(b) RETURN a".to_string(),
        ];
        let results = batch_mutate(&gqls);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(matches!(results[1], Err(GleaphError::ValidationError(_))));
    }

    // ── Metrics tests ────────────────────────────────────────────────────

    #[test]
    fn metrics_increment_on_query() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let before = crate::state::with_metrics(|m| m.clone());
        let result = crate::api::query_gql("MATCH (n) RETURN id(n) LIMIT 1".into(), None);
        assert!(result.is_ok(), "query failed: {:?}", result);
        let after = crate::state::with_metrics(|m| m.clone());
        assert_eq!(after.query_count, before.query_count + 1);
        assert_eq!(after.rejected_count, before.rejected_count);
    }

    #[test]
    fn metrics_increment_rejected_on_parse_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let before = crate::state::with_metrics(|m| m.clone());
        let _ = crate::api::query_gql("NOT VALID GQL !!!".into(), None);
        let after = crate::state::with_metrics(|m| m.clone());
        assert_eq!(after.rejected_count, before.rejected_count + 1);
        assert_eq!(after.query_count, before.query_count);
    }

    #[test]
    fn metrics_increment_on_mutation() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let before = crate::state::with_metrics(|m| m.clone());
        let _ = crate::api::mutate_gql(r#"INSERT (:User {name: 'A'})"#.into(), None);
        let after = crate::state::with_metrics(|m| m.clone());
        assert_eq!(after.mutation_count, before.mutation_count + 1);
        assert_eq!(after.rejected_count, before.rejected_count);
    }

    #[test]
    fn metrics_include_storage_stats_without_panic() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let m = crate::api::get_metrics();
        // stable_memory_bytes is 0 on non-wasm targets; just verify no panic
        assert_eq!(m.stable_memory_bytes, 0);
    }

    // ── Quota tests ─────────────────────────────────────────────────────

    #[test]
    fn quota_rejects_vertex_creation_over_limit() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        // Set quota: max 2 vertices
        crate::state::set_quota(gleaph_types::UsageQuota {
            max_vertices: 2,
            max_edges: 0,
        });
        // First two CREATE succeeds
        let r1 = crate::api::mutate_gql(r#"INSERT (:A)"#.into(), None);
        assert!(r1.is_ok(), "first create failed: {:?}", r1);
        assert!(crate::api::mutate_gql(r#"INSERT (:A)"#.into(), None).is_ok());
        // Third is rejected (already at quota)
        let err =
            crate::api::mutate_gql(r#"INSERT (:A)"#.into(), None).expect_err("should be quota err");
        assert!(matches!(err, GleaphError::UnsupportedFeature(_)));
        assert!(err.to_string().contains("vertex quota exceeded"));
        // Reset quota for other tests
        crate::state::set_quota(gleaph_types::UsageQuota {
            max_vertices: 0,
            max_edges: 0,
        });
    }

    #[test]
    fn quota_unlimited_when_max_is_zero() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        crate::state::set_quota(gleaph_types::UsageQuota {
            max_vertices: 0,
            max_edges: 0,
        });
        // With unlimited quota, creating multiple vertices is fine
        assert!(crate::api::mutate_gql(r#"INSERT (:B)"#.into(), None).is_ok());
        assert!(crate::api::mutate_gql(r#"INSERT (:B)"#.into(), None).is_ok());
        assert!(crate::api::mutate_gql(r#"INSERT (:B)"#.into(), None).is_ok());
    }

    // ── Graph type (§12) Graph type tests ─────────────────────────────────────────────────

    #[test]
    fn create_graph_type_stores_and_activates() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let outcome = mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]-> }",
        )
        .expect("should succeed");
        assert!(
            outcome.result.warnings[0]
                .message
                .contains("created and activated")
        );
        let gt = crate::state::get_graph_type("Social").expect("type should exist");
        assert_eq!(gt.node_labels, vec!["Company", "Person"]);
        assert_eq!(gt.edge_labels, vec!["KNOWS", "WORKS_AT"]);
        assert_eq!(
            crate::state::get_active_graph_type_name(),
            Some("Social".into())
        );
    }

    #[test]
    fn drop_graph_type_removes_and_deactivates() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE GRAPH TYPE Social { (:Person), -[:KNOWS]-> }").expect("create");
        assert!(crate::state::get_active_graph_type_name().is_some());
        let outcome = mutate_tracked("DROP GRAPH TYPE Social").expect("drop");
        assert!(outcome.result.warnings[0].message.contains("dropped"));
        assert!(crate::state::get_graph_type("Social").is_none());
        assert!(crate::state::get_active_graph_type_name().is_none());
    }

    #[test]
    fn drop_graph_type_nonexistent_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let result = mutate_tracked("DROP GRAPH TYPE NoSuch");
        let err = result.err().expect("should fail");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn create_allowed_labels_succeeds() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE GRAPH TYPE Social { (:Person), -[:KNOWS]-> }").expect("create type");
        let r = crate::api::mutate_gql(r#"INSERT (:Person {name: 'Alice'})"#.into(), None);
        assert!(r.is_ok(), "allowed label should succeed: {r:?}");
    }

    #[test]
    fn create_disallowed_node_label_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE GRAPH TYPE Social { (:Person), -[:KNOWS]-> }").expect("create type");
        let err = crate::api::mutate_gql(r#"INSERT (:Animal {name: 'Rex'})"#.into(), None)
            .expect_err("should reject");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("Animal"));
        assert!(err.to_string().contains("not allowed"));
    }

    #[test]
    fn create_disallowed_edge_label_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE GRAPH TYPE Social { (:Person), -[:KNOWS]-> }").expect("create type");
        // Create two valid vertices first
        crate::api::mutate_gql(r#"INSERT (:Person {name: 'A'})"#.into(), None).expect("v1");
        crate::api::mutate_gql(r#"INSERT (:Person {name: 'B'})"#.into(), None).expect("v2");
        let err = crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'X'})-[:HATES]->(:Person {name: 'Y'})"#.into(),
            None,
        )
        .expect_err("should reject");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("HATES"));
    }

    #[test]
    fn no_active_graph_type_allows_any_labels() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        // No graph type defined → open schema
        let r = crate::api::mutate_gql(r#"INSERT (:Anything)"#.into(), None);
        assert!(r.is_ok(), "open schema should allow any label: {r:?}");
    }

    #[test]
    fn drop_graph_type_removes_validation() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE GRAPH TYPE Strict { (:Person) }").expect("create type");
        // Animal is rejected while type is active
        let err = crate::api::mutate_gql(r#"INSERT (:Animal)"#.into(), None);
        assert!(err.is_err());
        // Drop the type
        mutate_tracked("DROP GRAPH TYPE Strict").expect("drop");
        // Now Animal is allowed (open schema)
        let r = crate::api::mutate_gql(r#"INSERT (:Animal)"#.into(), None);
        assert!(r.is_ok(), "should be allowed after drop: {r:?}");
    }

    #[test]
    fn create_graph_type_rejected_in_query_endpoint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = crate::api::query_gql("CREATE GRAPH TYPE T { (:A) }".into(), None)
            .expect_err("should reject in query");
        assert!(err.to_string().contains("mutation endpoint"));
    }

    // ── Schema (§12) Schema tests ─────────────────────────────────────────────────────

    #[test]
    fn create_schema_stores_name() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let outcome = mutate_tracked("CREATE SCHEMA myNamespace").expect("should succeed");
        assert!(outcome.result.warnings[0].message.contains("created"));
        assert!(crate::state::schema_exists("myNamespace"));
        assert_eq!(crate::state::list_schemas(), vec!["myNamespace"]);
    }

    #[test]
    fn drop_schema_removes_name() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE SCHEMA myNs").expect("create");
        assert!(crate::state::schema_exists("myNs"));
        let outcome = mutate_tracked("DROP SCHEMA myNs").expect("drop");
        assert!(outcome.result.warnings[0].message.contains("dropped"));
        assert!(!crate::state::schema_exists("myNs"));
    }

    #[test]
    fn create_schema_duplicate_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE SCHEMA myNs").expect("first create");
        let result = mutate_tracked("CREATE SCHEMA myNs");
        let err = result.err().expect("should fail on duplicate");
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn drop_schema_nonexistent_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let result = mutate_tracked("DROP SCHEMA noSuch");
        let err = result.err().expect("should fail");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn create_schema_rejected_in_query_endpoint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = crate::api::query_gql("CREATE SCHEMA myNs".into(), None)
            .expect_err("should reject in query");
        assert!(err.to_string().contains("mutation endpoint"));
    }

    #[test]
    fn drop_schema_rejected_in_query_endpoint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = crate::api::query_gql("DROP SCHEMA myNs".into(), None)
            .expect_err("should reject in query");
        assert!(err.to_string().contains("mutation endpoint"));
    }

    // ── P5 Prepared statements ──────────────────────────────────────────────

    #[test]
    fn prepared_query_basic() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        crate::api::mutate_gql(r#"INSERT (:User {name: 'Alice'})"#.into(), None).unwrap();
        crate::api::mutate_gql(r#"INSERT (:User {name: 'Bob'})"#.into(), None).unwrap();

        prepare_statement(
            "q1",
            "MATCH (n:User) WHERE n.name = $name RETURN n.name",
            None,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("name".into(), Value::Text("Alice".into()));
        let result = execute_prepared_query("q1", &params, None).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Text("Alice".into()));
    }

    #[test]
    fn prepared_query_missing_param() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement(
            "q2",
            "MATCH (n:User) WHERE n.name = $name RETURN n.name",
            None,
        )
        .unwrap();
        let params = std::collections::HashMap::new();
        let err = execute_prepared_query("q2", &params, None).expect_err("should fail");
        assert!(err.to_string().contains("undefined parameter"));
    }

    #[test]
    fn prepared_query_nonexistent() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let params = std::collections::HashMap::new();
        let err = execute_prepared_query("nope", &params, None).expect_err("should fail");
        assert!(err.to_string().contains("no prepared statement"));
    }

    #[test]
    fn prepared_drop() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("d1", "MATCH (n) RETURN n LIMIT 1", None).unwrap();
        assert!(drop_prepared("d1"));
        assert!(!drop_prepared("d1")); // already dropped
        let params = std::collections::HashMap::new();
        let err = execute_prepared_query("d1", &params, None).expect_err("should fail");
        assert!(err.to_string().contains("no prepared statement"));
    }

    #[test]
    fn prepared_overwrite() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("ow", "RETURN 1 AS x", None).unwrap();
        prepare_statement("ow", "RETURN 2 AS x", None).unwrap(); // overwrite
        let result = execute_prepared_query("ow", &std::collections::HashMap::new(), None).unwrap();
        assert_eq!(result.rows[0][0], Value::Int32(2));
    }

    #[test]
    fn prepared_list() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("la", "RETURN 1 AS a", None).unwrap();
        prepare_statement("lb", "RETURN 2 AS b", None).unwrap();
        let mut listed = list_prepared();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "la");
        assert_eq!(listed[1].name, "lb");
    }

    #[test]
    fn prepared_mutation_basic() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        // INSERT doesn't support $params in property maps, so use a static mutation.
        prepare_statement("m1", "INSERT (:User {name: 'Charlie'})", None).unwrap();
        let params = std::collections::HashMap::new();
        let outcome = execute_prepared_mutation("m1", &params)
            .map_err(|e| format!("{e}"))
            .expect("mutation failed");
        assert_eq!(outcome.result.affected_vertices, 1);
        let result = query_paged("MATCH (n:User) WHERE n.name = 'Charlie' RETURN n.name").unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn prepared_mutation_wrong_endpoint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("mut_q", "MATCH (n) RETURN n LIMIT 1", None).unwrap();
        let params = std::collections::HashMap::new();
        let err = execute_prepared_mutation("mut_q", &params)
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("read queries"));
    }

    #[test]
    fn prepared_query_wrong_endpoint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("q_mut", "INSERT (:X)", None).unwrap();
        let params = std::collections::HashMap::new();
        let err = execute_prepared_query("q_mut", &params, None).expect_err("should fail");
        assert!(err.to_string().contains("execute_prepared_mutation"));
    }

    #[test]
    fn prepared_cache_limit() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        for i in 0..256 {
            prepare_statement(&format!("s{i}"), "RETURN 1 AS x", None).unwrap();
        }
        let err = prepare_statement("overflow", "RETURN 1 AS x", None).expect_err("should fail");
        assert!(err.to_string().contains("cache full"));
    }

    #[test]
    fn prepared_mutation_with_params() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})"#.into(),
            None,
        )
        .unwrap();
        // SET requires at least 1 hop in MATCH clause.
        prepare_statement(
            "update_name",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = $src SET b.name = $new_name",
            None,
        )
        .unwrap();
        let mut params = std::collections::HashMap::new();
        params.insert("src".into(), Value::Text("Alice".into()));
        params.insert("new_name".into(), Value::Text("Carol".into()));
        execute_prepared_mutation("update_name", &params)
            .map_err(|e| format!("{e}"))
            .expect("mutation failed");
        let result = query_paged(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Alice' RETURN b.name",
        )
        .unwrap();
        assert_eq!(result.rows[0][0], Value::Text("Carol".into()));
    }

    // ── PreparedStatementInfo metadata tests ───────────────────────────────────

    #[test]
    fn prepared_info_query_metadata() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement(
            "qi",
            "MATCH (n:User) WHERE n.name = $name RETURN n.name AS nm, id(n) AS nid",
            None,
        )
        .unwrap();
        assert_eq!(info.name, "qi");
        assert!(matches!(info.kind, gleaph_types::PreparedKind::Query));
        assert_eq!(
            info.parameters,
            vec![gleaph_types::PreparedParameterInfo {
                name: "name".into(),
                required: true,
                types: vec![],
                inferred: true,
            }]
        );
        assert_eq!(info.columns, vec!["nm", "nid"]);
        assert!(!info.requires_caller);
        assert!(info.source.contains("$name"));
    }

    #[test]
    fn prepared_info_mutation_metadata() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement("mi", "INSERT (:User {name: 'Alice'})", None).unwrap();
        assert_eq!(info.name, "mi");
        assert!(matches!(info.kind, gleaph_types::PreparedKind::Mutation));
        assert!(info.parameters.is_empty());
        assert!(info.columns.is_empty()); // mutations have no RETURN
        assert!(!info.requires_caller);
    }

    #[test]
    fn prepared_info_requires_caller() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement(
            "ci",
            "MATCH (n:User) WHERE n.owner = caller() RETURN n.name AS name",
            None,
        )
        .unwrap();
        assert!(info.requires_caller);
        assert_eq!(info.columns, vec!["name"]);
    }

    #[test]
    fn prepared_info_no_caller() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement("nc", "RETURN 42 AS val", None).unwrap();
        assert!(!info.requires_caller);
    }

    #[test]
    fn prepared_info_multiple_params() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement(
            "mp",
            "MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = $src AND b.age > $min_age RETURN b.name AS name",
            None,
        )
        .unwrap();
        // BTreeSet ensures sorted order
        assert_eq!(
            info.parameters,
            vec![
                gleaph_types::PreparedParameterInfo {
                    name: "min_age".into(),
                    required: true,
                    types: vec![],
                    inferred: true,
                },
                gleaph_types::PreparedParameterInfo {
                    name: "src".into(),
                    required: true,
                    types: vec![],
                    inferred: true,
                }
            ]
        );
        assert_eq!(info.columns, vec!["name"]);
    }

    #[test]
    fn prepare_statement_rejects_impossible_pattern_queries() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:WORKS_AT]->, (:Person)-[:WORKS_AT]->(:Company) }",
        )
        .expect("create graph type");
        let err = prepare_statement(
            "bad_q",
            "MATCH (a:Company)-[:WORKS_AT]->(b:Company) RETURN a, b",
            None,
        )
        .expect_err("should reject impossible prepared query");
        assert!(err.to_string().contains("cannot prepare query"));
        assert!(err.to_string().contains("pattern endpoint contradiction"));
    }

    #[test]
    fn prepared_list_excludes_rejected_impossible_pattern_queries() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:WORKS_AT]->, (:Person)-[:WORKS_AT]->(:Company) }",
        )
        .expect("create graph type");
        let _ = prepare_statement(
            "bad_q",
            "MATCH (a:Company)-[:WORKS_AT]->(b:Company) RETURN a, b",
            None,
        )
        .expect_err("should reject impossible prepared query");
        let listed = list_prepared();
        assert!(listed.into_iter().all(|info| info.name != "bad_q"));
    }

    #[test]
    fn prepared_info_optional_params_via_null_union() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement(
            "optp",
            "MATCH (u:User) WHERE ($min_age :: INT | NULL IS NULL OR u.age >= $min_age :: INT | NULL) AND u.name = $name RETURN u.name AS name",
            None,
        )
        .unwrap();
        assert_eq!(
            info.parameters,
            vec![
                gleaph_types::PreparedParameterInfo {
                    name: "min_age".into(),
                    required: false,
                    types: vec![
                        gleaph_types::PreparedValueType::Int32,
                        gleaph_types::PreparedValueType::Null
                    ],
                    inferred: false,
                },
                gleaph_types::PreparedParameterInfo {
                    name: "name".into(),
                    required: true,
                    types: vec![],
                    inferred: true,
                }
            ]
        );
    }

    #[test]
    fn prepared_list_returns_info() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("li_a", "RETURN 1 AS x", None).unwrap();
        prepare_statement("li_b", "INSERT (:X)", None).unwrap();
        let mut listed = list_prepared();
        listed.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "li_a");
        assert!(matches!(listed[0].kind, gleaph_types::PreparedKind::Query));
        assert_eq!(listed[0].columns, vec!["x"]);
        assert_eq!(listed[1].name, "li_b");
        assert!(matches!(
            listed[1].kind,
            gleaph_types::PreparedKind::Mutation
        ));
        assert!(listed[1].columns.is_empty());
    }

    #[test]
    fn caller_injected_in_prepared_query() {
        // execute_prepared_query internally calls inject_caller(), which on
        // non-wasm32 sets caller() to the anonymous principal.
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement("cq", "RETURN caller() AS c", None).unwrap();
        let result = execute_prepared_query("cq", &std::collections::HashMap::new(), None).unwrap();
        assert_eq!(result.columns, vec!["c"]);
        assert_eq!(
            result.rows[0][0],
            Value::Principal(candid::Principal::anonymous())
        );
    }

    #[test]
    fn prepared_query_impossible_pattern_never_registers() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:WORKS_AT]->, (:Person)-[:WORKS_AT]->(:Company) }",
        )
        .expect("create graph type");
        let _ = prepare_statement(
            "bad_q",
            "MATCH (a:Company)-[:WORKS_AT]->(b:Company) RETURN a, b",
            None,
        )
        .expect_err("should reject impossible prepared query");
        let err = execute_prepared_query("bad_q", &std::collections::HashMap::new(), None)
            .expect_err("query should not exist");
        assert!(err.to_string().contains("no prepared statement"));
    }

    #[test]
    fn caller_in_match_where() {
        // Test that caller() works in a MATCH WHERE clause for filtering.
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let anon = candid::Principal::anonymous();
        // Insert a doc and set owner via parameterized mutation
        mutate_tracked("INSERT (:Owner)-[:OWNS]->(:Doc {title: 'owned'})").unwrap();
        mutate_tracked("INSERT (:Doc {title: 'other'})").unwrap();
        // Use SET with a parameter to assign the principal as owner
        let mut params = std::collections::HashMap::new();
        params.insert("p".into(), Value::Principal(anon));
        crate::api::mutate_gql(
            "MATCH (:Owner)-[:OWNS]->(d:Doc) WHERE d.title = 'owned' SET d.owner = $p".into(),
            Some(params.into_iter().collect()),
        )
        .unwrap();
        // Query via caller() in WHERE — caller() returns anonymous on non-wasm32
        set_caller(Value::Principal(candid::Principal::anonymous()));
        let result =
            query_paged("MATCH (d:Doc) WHERE d.owner = caller() RETURN d.title AS t").unwrap();
        clear_caller();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Text("owned".into()));
    }

    #[test]
    fn prepared_info_caller_in_return() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info = prepare_statement("cr", "RETURN caller() AS me", None).unwrap();
        assert!(info.requires_caller);
        assert_eq!(info.columns, vec!["me"]);
    }

    #[test]
    fn prepared_dynamic_sort_rejects_static_order_by() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = prepare_statement(
            "dyn_bad",
            "MATCH (u:User) RETURN u.name ORDER BY u.name",
            Some(gleaph_types::PreparedOptions {
                description: None,
                allowed_sorts: vec![gleaph_types::PreparedSortKey {
                    key: "name".into(),
                    expr: "u.name".into(),
                }],
                default_sort: None,
            }),
        )
        .expect_err("should reject");
        assert!(
            err.to_string()
                .contains("cannot define both ORDER BY in GQL and dynamic sort options")
        );
    }

    #[test]
    fn prepared_dynamic_sort_rejects_mutation_options() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = prepare_statement(
            "dyn_mut",
            "INSERT (:User {name: 'Alice'})",
            Some(gleaph_types::PreparedOptions {
                description: None,
                allowed_sorts: vec![gleaph_types::PreparedSortKey {
                    key: "name".into(),
                    expr: "n.name".into(),
                }],
                default_sort: None,
            }),
        )
        .expect_err("should reject");
        assert!(
            err.to_string()
                .contains("only supported for query prepared statements")
        );
    }

    #[test]
    fn prepared_dynamic_sort_default_and_override() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        crate::api::mutate_gql(r#"INSERT (:User {name: 'Bob', age: 20})"#.into(), None).unwrap();
        crate::api::mutate_gql(r#"INSERT (:User {name: 'Alice', age: 30})"#.into(), None).unwrap();
        crate::api::mutate_gql(r#"INSERT (:User {name: 'Carol', age: 25})"#.into(), None).unwrap();

        let info = prepare_statement(
            "dyn_users",
            "MATCH (u:User) RETURN u.name AS name, u.age AS age",
            Some(gleaph_types::PreparedOptions {
                description: Some("List users with dynamic sort.".into()),
                allowed_sorts: vec![
                    gleaph_types::PreparedSortKey {
                        key: "name".into(),
                        expr: "u.name".into(),
                    },
                    gleaph_types::PreparedSortKey {
                        key: "age".into(),
                        expr: "u.age".into(),
                    },
                ],
                default_sort: Some(vec![gleaph_types::PreparedSortSpec {
                    key: "age".into(),
                    descending: true,
                    nulls_first: None,
                }]),
            }),
        )
        .unwrap();
        assert_eq!(
            info.description.as_deref(),
            Some("List users with dynamic sort.")
        );
        assert_eq!(info.allowed_sorts.len(), 2);
        assert_eq!(
            info.default_sort,
            Some(vec![gleaph_types::PreparedSortSpec {
                key: "age".into(),
                descending: true,
                nulls_first: None,
            }])
        );

        let default_sorted =
            execute_prepared_query("dyn_users", &std::collections::HashMap::new(), None).unwrap();
        let default_names: Vec<String> = default_sorted
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Text(name) => name.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect();
        assert_eq!(default_names, vec!["Alice", "Carol", "Bob"]);

        let name_sorted = execute_prepared_query(
            "dyn_users",
            &std::collections::HashMap::new(),
            Some(vec![gleaph_types::PreparedSortSpec {
                key: "name".into(),
                descending: false,
                nulls_first: None,
            }]),
        )
        .unwrap();
        let name_order: Vec<String> = name_sorted
            .rows
            .iter()
            .map(|row| match &row[0] {
                Value::Text(name) => name.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect();
        assert_eq!(name_order, vec!["Alice", "Bob", "Carol"]);
    }

    #[test]
    fn prepared_dynamic_sort_rejects_unknown_key() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        prepare_statement(
            "dyn_key",
            "MATCH (u:User) RETURN u.name AS name",
            Some(gleaph_types::PreparedOptions {
                description: None,
                allowed_sorts: vec![gleaph_types::PreparedSortKey {
                    key: "name".into(),
                    expr: "u.name".into(),
                }],
                default_sort: None,
            }),
        )
        .unwrap();
        let err = execute_prepared_query(
            "dyn_key",
            &std::collections::HashMap::new(),
            Some(vec![gleaph_types::PreparedSortSpec {
                key: "age".into(),
                descending: false,
                nulls_first: None,
            }]),
        )
        .expect_err("should reject");
        assert!(err.to_string().contains("unknown prepared sort key"));
    }

    // ── Property type validation tests ────────────────────────────────────────

    #[test]
    fn property_type_validation_insert_matching_type_succeeds() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (PersonType :Person { name :: TEXT NOT NULL, age :: INT }) }",
        )
        .expect("create type");
        let r = crate::api::mutate_gql(r#"INSERT (:Person {name: 'Alice', age: 30})"#.into(), None);
        assert!(r.is_ok(), "matching types should succeed: {r:?}");
    }

    #[test]
    fn property_type_validation_wrong_type_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (PersonType :Person { name :: TEXT NOT NULL, age :: INT }) }",
        )
        .expect("create type");
        let err = crate::api::mutate_gql(r#"INSERT (:Person {name: 123, age: 30})"#.into(), None)
            .expect_err("should reject");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("name"));
        assert!(err.to_string().contains("INT"));
        assert!(err.to_string().contains("Text"));
    }

    #[test]
    fn property_type_validation_missing_required_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (PersonType :Person { name :: TEXT NOT NULL, age :: INT }) }",
        )
        .expect("create type");
        let err = crate::api::mutate_gql(r#"INSERT (:Person {age: 30})"#.into(), None)
            .expect_err("should reject");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("required"));
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn property_type_validation_undefined_property_allowed() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (PersonType :Person { name :: TEXT NOT NULL }) }",
        )
        .expect("create type");
        let r = crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'Alice', extra: 'whatever'})"#.into(),
            None,
        );
        assert!(r.is_ok(), "open schema should allow extra props: {r:?}");
    }

    #[test]
    fn property_type_validation_set_wrong_type_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), -[:KNOWS]->, (PersonType :Person { name :: TEXT NOT NULL, age :: INT }) }",
        )
        .expect("create type");
        crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'Alice', age: 30})-[:KNOWS]->(:Person {name: 'Bob', age: 25})"#.into(),
            None,
        )
        .expect("insert");
        let result = crate::api::mutate_gql(
            r#"MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = 'Alice' SET b.age = 'not a number'"#.into(),
            None,
        );
        let err = result.expect_err("should reject");
        assert!(
            matches!(err, GleaphError::ValidationError(_)),
            "expected ValidationError, got: {err:?}"
        );
        assert!(err.to_string().contains("age"));
    }

    #[test]
    fn property_type_validation_no_graph_type_allows_all() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        // No graph type → everything is allowed
        let r = crate::api::mutate_gql(r#"INSERT (:Whatever {x: 123, y: 'abc'})"#.into(), None);
        assert!(r.is_ok(), "no graph type should allow everything: {r:?}");
    }

    // ── Edge type endpoint enforcement ─────────────────────────────────────────

    #[test]
    fn edge_type_valid_endpoints_succeeds() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]->, (:Person)-[:KNOWS]->(:Person), (:Person)-[:WORKS_AT]->(:Company) }",
        )
        .expect("create type");
        // Valid: Person -KNOWS-> Person
        let r = crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'A'})-[:KNOWS]->(:Person {name: 'B'})"#.into(),
            None,
        );
        assert!(r.is_ok(), "valid endpoints should succeed: {r:?}");
    }

    #[test]
    fn edge_type_invalid_endpoints_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]->, (:Person)-[:KNOWS]->(:Person), (:Person)-[:WORKS_AT]->(:Company) }",
        )
        .expect("create type");
        // Invalid: Company -KNOWS-> Person (source must be Person)
        let err = crate::api::mutate_gql(
            r#"INSERT (:Company {name: 'X'})-[:KNOWS]->(:Person {name: 'Y'})"#.into(),
            None,
        )
        .expect_err("should reject invalid endpoints");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("does not allow endpoint"));
    }

    #[test]
    fn edge_type_undefined_label_open_schema() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]->, -[:LIKES]->, (:Person)-[:KNOWS]->(:Person) }",
        )
        .expect("create type");
        // LIKES has no edge type defined → open schema, any endpoints allowed
        let r = crate::api::mutate_gql(
            r#"INSERT (:Company {name: 'X'})-[:LIKES]->(:Person {name: 'Y'})"#.into(),
            None,
        );
        assert!(
            r.is_ok(),
            "undefined edge type should allow any endpoints: {r:?}"
        );
    }

    #[test]
    fn edge_type_multiple_defs_one_matches() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:WORKS_AT]->, (:Person)-[:WORKS_AT]->(:Company), (:Company)-[:WORKS_AT]->(:Company) }",
        )
        .expect("create type");
        // Company -> Company matches the second definition
        let r = crate::api::mutate_gql(
            r#"INSERT (:Company {name: 'X'})-[:WORKS_AT]->(:Company {name: 'Y'})"#.into(),
            None,
        );
        assert!(r.is_ok(), "should match second edge type def: {r:?}");
    }

    #[test]
    fn edge_type_property_type_mismatch_rejected() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), -[:KNOWS]->, (:Person)-[:KNOWS { since :: INT }]->(:Person) }",
        )
        .expect("create type");
        // Wrong type: since should be INT, not TEXT
        let err = crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'A'})-[:KNOWS {since: 'yesterday'}]->(:Person {name: 'B'})"#
                .into(),
            None,
        )
        .expect_err("should reject type mismatch");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("since"));
    }

    #[test]
    fn edge_type_multi_endpoint_labels() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            "CREATE GRAPH TYPE Social { (:Person), (:Contractor), (:Company), (:Startup), -[:WORKS_AT]->, (:Person | :Contractor)-[:WORKS_AT]->(:Company | :Startup) }",
        )
        .expect("create type");
        // Contractor -> Startup should be valid
        let r = crate::api::mutate_gql(
            r#"INSERT (:Contractor {name: 'A'})-[:WORKS_AT]->(:Startup {name: 'B'})"#.into(),
            None,
        );
        assert!(r.is_ok(), "multi-endpoint should match: {r:?}");
        // Person -> Person should fail (Person is not in to_types)
        let err = crate::api::mutate_gql(
            r#"INSERT (:Person {name: 'X'})-[:WORKS_AT]->(:Person {name: 'Y'})"#.into(),
            None,
        )
        .expect_err("should reject");
        assert!(matches!(err, GleaphError::ValidationError(_)));
    }

    // ── W-D5 DESCRIBE GRAPH TYPE tests ─────────────────────────────────────

    #[test]
    fn describe_graph_type_returns_schema_rows() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked(
            r#"CREATE GRAPH TYPE Social {
                (:Person), (:Company), -[:KNOWS]->, -[:WORKS_AT]->,
                (PersonType :Person { name :: TEXT NOT NULL, age :: INT }),
                (:Person)-[:KNOWS { since :: INT }]->(:Person)
            }"#,
        )
        .expect("create graph type");

        let result = query("DESCRIBE GRAPH TYPE Social").expect("describe should succeed");
        assert_eq!(
            result.columns,
            vec![
                "kind",
                "name",
                "label",
                "labels",
                "from_types",
                "to_types",
                "properties"
            ]
        );
        // 2 node labels + 2 edge labels + 1 node type + 1 edge type = 6 rows
        assert_eq!(result.rows.len(), 6);

        // Check node label rows
        assert_eq!(result.rows[0][0], Value::Text("node".into()));
        assert_eq!(result.rows[1][0], Value::Text("node".into()));

        // Check edge label rows
        assert_eq!(result.rows[2][0], Value::Text("edge".into()));
        assert_eq!(result.rows[3][0], Value::Text("edge".into()));

        // Check node_type row
        assert_eq!(result.rows[4][0], Value::Text("node_type".into()));
        assert_eq!(result.rows[4][1], Value::Text("PersonType".into()));
        // labels should be [Person]
        assert_eq!(
            result.rows[4][3],
            Value::List(vec![Value::Text("Person".into())])
        );
        // properties should list name::TEXT NOT NULL, age::INT
        if let Value::List(ref props) = result.rows[4][6] {
            assert_eq!(props.len(), 2);
            assert_eq!(props[0], Value::Text("name::TEXT NOT NULL".into()));
            assert_eq!(props[1], Value::Text("age::INT32".into()));
        } else {
            panic!("expected properties list, got {:?}", result.rows[4][6]);
        }

        // Check edge_type row
        assert_eq!(result.rows[5][0], Value::Text("edge_type".into()));
        assert_eq!(result.rows[5][1], Value::Text("_KNOWS".into()));
        assert_eq!(result.rows[5][2], Value::Text("KNOWS".into()));
        assert_eq!(
            result.rows[5][4],
            Value::List(vec![Value::Text("Person".into())])
        );
        assert_eq!(
            result.rows[5][5],
            Value::List(vec![Value::Text("Person".into())])
        );
    }

    #[test]
    fn describe_graph_type_nonexistent_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = query("DESCRIBE GRAPH TYPE NoSuchType").expect_err("should fail");
        assert!(matches!(err, GleaphError::ExecutionError(_)));
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn describe_graph_type_rejected_in_mutation_endpoint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = mutate_tracked("DESCRIBE GRAPH TYPE Social")
            .err()
            .expect("should fail");
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("read-only"));
    }

    // ── §18.9 Phase 3: Strict type checking ──────────────────────────────

    #[test]
    fn set_type_check_strict_rejects_mismatch() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        // Create a graph type with a typed property.
        mutate_tracked(
            "CREATE GRAPH TYPE T { (:Person), -[:KNOWS]->, (PersonType :Person { age :: INT }) }",
        )
        .expect("create graph type");
        // Enable strict mode.
        let outcome = mutate_tracked("SET TYPE CHECK STRICT").expect("set strict");
        assert!(outcome.result.warnings[0].message.contains("STRICT"));
        // Query with type mismatch should fail.
        let err = query("MATCH (n:Person) RETURN n.age + 'hello'")
            .expect_err("should fail in strict mode");
        assert!(err.to_string().contains("type error"), "got: {err}");
        // Disable strict mode.
        mutate_tracked("SET TYPE CHECK WARNING").expect("set warning");
        // Same query should now succeed (with warning).
        let result = query("MATCH (n:Person) RETURN n.age + 'hello'").expect("should succeed");
        assert!(!result.warnings.is_empty(), "should have warnings");
    }

    #[test]
    fn show_settings_displays_type_check_mode() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let result = query("SHOW SETTINGS").expect("show settings");
        assert_eq!(result.columns, vec!["setting", "value"]);
        assert_eq!(result.rows[0][0], Value::Text("type_check_mode".into()));
        assert_eq!(result.rows[0][1], Value::Text("WARNING".into()));
        // Set to strict and check again.
        mutate_tracked("SET TYPE CHECK STRICT").expect("set strict");
        let result = query("SHOW SETTINGS").expect("show settings");
        assert_eq!(result.rows[0][1], Value::Text("STRICT".into()));
        // Reset for other tests.
        crate::state::set_strict_type_check(false);
    }

    #[test]
    fn strict_mode_passes_valid_query() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        crate::state::set_strict_type_check(true);
        // No schema → all properties Unknown → no error.
        let result = query("MATCH (n:Person) RETURN n.age + 42").expect("should pass");
        assert!(result.warnings.is_empty());
        crate::state::set_strict_type_check(false);
    }

    // ── Parameter inference conflict diagnostics ───────────────────────

    #[test]
    fn prepare_with_inference_conflict_emits_warning() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        // $x is compared with both an INT literal and a TEXT literal → conflict.
        let info = prepare_statement(
            "conflict_q",
            "MATCH (u:User) WHERE 42 = $x AND 'hello' = $x RETURN u",
            None,
        )
        .unwrap();
        assert!(
            info.type_warnings.iter().any(|w| w.kind
                == gleaph_types::TypeDiagnosticKind::ParameterInferenceConflict
                && w.message.contains("$x")),
            "expected ParameterInferenceConflict warning for $x, got: {:?}",
            info.type_warnings,
        );
        // Parameter types should be empty due to conflict.
        let x_param = info.parameters.iter().find(|p| p.name == "x").unwrap();
        assert!(x_param.types.is_empty(), "conflict should clear types");
    }

    #[test]
    fn prepare_strict_mode_rejects_inference_conflict() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        crate::state::set_strict_type_check(true);
        let err = prepare_statement(
            "strict_conflict_q",
            "MATCH (u:User) WHERE 42 = $x AND 'hello' = $x RETURN u",
            None,
        )
        .expect_err("strict mode should reject inference conflict");
        assert!(
            err.to_string().contains("strict type check"),
            "expected strict type check error, got: {err}"
        );
        assert!(
            err.to_string().contains("$x"),
            "error should mention parameter name, got: {err}"
        );
        crate::state::set_strict_type_check(false);
    }

    #[test]
    fn prepare_no_conflict_no_warning() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let info =
            prepare_statement("clean_q", "MATCH (u:User) WHERE 42 > $min RETURN u", None).unwrap();
        assert!(
            !info
                .type_warnings
                .iter()
                .any(|w| w.kind == gleaph_types::TypeDiagnosticKind::ParameterInferenceConflict),
            "should have no ParameterInferenceConflict warnings"
        );
    }

    // ── §12 CONSTRAINT tests ────────────────────────────────────────────

    #[test]
    fn create_and_drop_constraint() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let outcome =
            mutate_tracked("CREATE CONSTRAINT uniq_name ON (:Person) ASSERT name IS UNIQUE")
                .expect("create constraint");
        assert!(outcome.result.warnings[0].message.contains("created"));
        assert!(crate::state::get_constraint("uniq_name").is_some());

        // SHOW CONSTRAINTS
        let result = query("SHOW CONSTRAINTS").expect("show");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Text("uniq_name".into()));
        assert_eq!(result.rows[0][1], Value::Text("UNIQUE".into()));

        // DROP
        let outcome = mutate_tracked("DROP CONSTRAINT uniq_name").expect("drop");
        assert!(outcome.result.warnings[0].message.contains("dropped"));
        assert!(crate::state::get_constraint("uniq_name").is_none());
    }

    #[test]
    fn drop_constraint_nonexistent_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        let err = mutate_tracked("DROP CONSTRAINT no_such")
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn unique_constraint_blocks_duplicate_on_create() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(16, 0).expect("init state");
        // Insert first vertex.
        crate::api::mutate_gql("INSERT (:Person {name: 'Alice'})".into(), None).expect("insert");
        // Create constraint — should succeed (no duplicates yet).
        mutate_tracked("CREATE CONSTRAINT uniq_name ON (:Person) ASSERT name IS UNIQUE")
            .expect("create constraint");
        // Insert duplicate — should fail.
        let err = crate::api::mutate_gql("INSERT (:Person {name: 'Alice'})".into(), None)
            .expect_err("should reject duplicate");
        assert!(err.to_string().contains("UNIQUE constraint"));
        // Insert different value — should succeed.
        crate::api::mutate_gql("INSERT (:Person {name: 'Bob'})".into(), None)
            .expect("different value should succeed");
        // Cleanup
        crate::state::remove_constraint("uniq_name");
    }

    #[test]
    fn unique_constraint_creation_fails_on_existing_duplicates() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(16, 0).expect("init state");
        crate::api::mutate_gql("INSERT (:Person {name: 'Alice'})".into(), None).expect("insert 1");
        crate::api::mutate_gql("INSERT (:Person {name: 'Alice'})".into(), None).expect("insert 2");
        let err = mutate_tracked("CREATE CONSTRAINT uniq_name ON (:Person) ASSERT name IS UNIQUE")
            .err()
            .expect("should fail on existing duplicates");
        assert!(err.to_string().contains("UNIQUE constraint violation"));
    }

    #[test]
    fn not_null_constraint_blocks_missing_property() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(16, 0).expect("init state");
        mutate_tracked("CREATE CONSTRAINT nn_name ON (:Person) ASSERT name IS NOT NULL")
            .expect("create constraint");
        // Insert without the required property — should fail.
        let err = crate::api::mutate_gql("INSERT (:Person {age: 30})".into(), None)
            .expect_err("should reject missing property");
        assert!(err.to_string().contains("NOT NULL constraint"));
        // Insert with the property — should succeed.
        crate::api::mutate_gql("INSERT (:Person {name: 'Alice'})".into(), None)
            .expect("should succeed with property");
        // Cleanup
        crate::state::remove_constraint("nn_name");
    }

    #[test]
    fn not_null_constraint_creation_fails_on_existing_violations() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(16, 0).expect("init state");
        crate::api::mutate_gql("INSERT (:Person {age: 30})".into(), None)
            .expect("insert without name");
        let err = mutate_tracked("CREATE CONSTRAINT nn_name ON (:Person) ASSERT name IS NOT NULL")
            .err()
            .expect("should fail on existing violation");
        assert!(err.to_string().contains("NOT NULL constraint violation"));
    }

    #[test]
    fn constraint_duplicate_name_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        init_state(8, 0).expect("init state");
        mutate_tracked("CREATE CONSTRAINT c1 ON (:Person) ASSERT name IS UNIQUE")
            .expect("first create");
        let err = mutate_tracked("CREATE CONSTRAINT c1 ON (:Person) ASSERT age IS UNIQUE")
            .err()
            .expect("should reject duplicate name");
        assert!(err.to_string().contains("already exists"));
        crate::state::remove_constraint("c1");
    }
}
