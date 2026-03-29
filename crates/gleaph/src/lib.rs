//! Gleaph integration facade.
//!
//! This crate exposes API-oriented request/response DTOs on top of the parser,
//! planner, and executor crates.
//!
//! `ApiValue` is the stable serialization boundary for external callers.
//! The current conversion rules are:
//!
//! - `Int8`/`Int16`/`Int32` are widened to `ApiValue::Int64`
//! - `Uint8`/`Uint16`/`Uint32` are widened to `ApiValue::Uint64`
//! - `Int256`, `Uint256`, and `Decimal` are serialized as decimal strings
//! - temporal values are serialized as structured records
//! - `Record` values are serialized as string-keyed maps
//! - `Extension` values are serialized as `{ type_name, display }`
//!
//! The reverse conversion accepts those encodings and falls back to
//! `Value::Text` when a string-encoded wide numeric value cannot be parsed.

mod auth;
mod prepared;
mod service;

use gleaph_gql::ast::{LinearQueryStatement, Statement, StatementBlock};
use gleaph_gql::{GqlError, parser};
use gleaph_gql::Value;
use gleaph_gql_executor::{ExecutionContext, ExecutionError, ExecutionResult, execute_plan_with_context};
use gleaph_gql_planner::{
    GraphStats, PlanBuildOutput, PlanSummary, PlannerError, build_block_plan_output,
    build_plan_output,
};
use gleaph_graph_kernel::{GraphRead, GraphWrite};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

pub use auth::{AccessLevel, AclEntry, AuthContext, Operation, PermissionChecker};
pub use prepared::{
    PreparedColumnInfo, PreparedParameterInfo, PreparedRegistry, PreparedStatementInfo,
    PreparedStatementKind,
};
pub use service::GleaphService;

#[derive(Clone, Debug)]
pub struct QueryRunOutput {
    pub plan: PlanBuildOutput,
    pub execution: ExecutionResult,
}

#[derive(Clone, Debug, Default)]
pub struct QueryRequest {
    pub query: String,
    pub params: BTreeMap<String, Value>,
}

/// Planner response for internal Rust callers.
#[derive(Clone, Debug)]
pub struct PlanResponse {
    pub explain: String,
    pub summary: PlanSummary,
}

/// Query execution response for internal Rust callers.
#[derive(Clone, Debug)]
pub struct QueryResponse {
    pub explain: String,
    pub plan_summary: PlanSummary,
    pub execution: ExecutionResult,
}

/// Stable serialized value type for external API boundaries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ApiValue {
    Null,
    Bool(bool),
    Int64(i64),
    Uint64(u64),
    Int128(i128),
    Uint128(u128),
    Int256(String),
    Uint256(String),
    Float64(f64),
    Decimal(String),
    Text(String),
    Bytes(Vec<u8>),
    Date(i32),
    Time(u64),
    LocalTime(u64),
    DateTime { seconds: i64, nanos: u32 },
    LocalDateTime { seconds: i64, nanos: u32 },
    ZonedDateTime { seconds: i64, nanos: u32, offset_seconds: i32 },
    ZonedTime { nanos: u64, offset_seconds: i32 },
    Duration { months: i32, nanos: i64 },
    List(Vec<ApiValue>),
    Path(Vec<ApiPathElement>),
    Record(BTreeMap<String, ApiValue>),
    Extension { type_name: String, display: String },
}

/// Serialized representation of a path element.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ApiPathElement {
    Vertex(u64),
    Edge {
        src: u64,
        dst: u64,
        label: Option<String>,
    },
}

/// External query request DTO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiQueryRequest {
    pub query: String,
    pub params: BTreeMap<String, ApiValue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiAuthContext {
    pub caller: Option<String>,
    pub is_controller: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiPreparedParameterInfo {
    pub name: String,
    pub required: bool,
    pub nullable: bool,
    pub inferred: bool,
    pub type_hints: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiPreparedColumnInfo {
    pub name: String,
    pub expr: String,
    pub aliased: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiPreparedStatementKind {
    Query,
    Mutation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPreparedStatementInfo {
    pub name: String,
    pub kind: ApiPreparedStatementKind,
    pub source: String,
    pub columns: Vec<ApiPreparedColumnInfo>,
    pub parameters: Vec<ApiPreparedParameterInfo>,
    pub type_warnings: Vec<ApiTypeDiagnostic>,
    pub explain: String,
    pub summary: ApiPlanSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiPrepareRequest {
    pub name: String,
    pub query: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPrepareResponse {
    pub prepared: ApiPreparedStatementInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiExecutePreparedRequest {
    pub name: String,
    pub params: BTreeMap<String, ApiValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiDropPreparedRequest {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiDropPreparedResponse {
    pub dropped: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiListPreparedResponse {
    pub statements: Vec<ApiPreparedStatementInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPlanEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiQueryRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiExecuteEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiQueryRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPrepareEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiPrepareRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiExecutePreparedEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiExecutePreparedRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiDropPreparedEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiDropPreparedRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiListPreparedEndpointRequest {
    pub auth: ApiAuthContext,
}

/// Serialized planner summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPlanSummary {
    pub estimated_rows: Option<f64>,
    pub estimated_cost: Option<f64>,
    pub has_dml: bool,
    pub dml_error_count: usize,
    pub dml_warning_count: usize,
    pub type_warning_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiDiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiTypeDiagnostic {
    pub code: Option<String>,
    pub message: String,
    pub span_start: u32,
    pub span_end: u32,
    pub severity: ApiDiagnosticSeverity,
}

/// Serialized execution summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiExecutionSummary {
    pub row_count: usize,
    pub warning_count: usize,
    pub had_dml: bool,
}

/// Serialized execution result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiExecutionResult {
    pub rows: Vec<BTreeMap<String, ApiValue>>,
    pub warnings: Vec<String>,
    pub summary: ApiExecutionSummary,
}

/// External planner response DTO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiPlanResponse {
    pub explain: String,
    pub summary: ApiPlanSummary,
}

/// External query response DTO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiQueryResponse {
    pub explain: String,
    pub plan_summary: ApiPlanSummary,
    pub execution: ApiExecutionResult,
}

impl From<&gleaph_gql::types::PathElement> for ApiPathElement {
    fn from(value: &gleaph_gql::types::PathElement) -> Self {
        match value {
            gleaph_gql::types::PathElement::Vertex(id) => Self::Vertex(*id),
            gleaph_gql::types::PathElement::Edge { src, dst, label } => Self::Edge {
                src: *src,
                dst: *dst,
                label: label.clone(),
            },
        }
    }
}

impl From<&Value> for ApiValue {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int8(v) => Self::Int64((*v).into()),
            Value::Int16(v) => Self::Int64((*v).into()),
            Value::Int32(v) => Self::Int64((*v).into()),
            Value::Int64(v) => Self::Int64(*v),
            Value::Int128(v) => Self::Int128(*v),
            Value::Int256(v) => Self::Int256(v.to_string()),
            Value::Uint8(v) => Self::Uint64((*v).into()),
            Value::Uint16(v) => Self::Uint64((*v).into()),
            Value::Uint32(v) => Self::Uint64((*v).into()),
            Value::Uint64(v) => Self::Uint64(*v),
            Value::Uint128(v) => Self::Uint128(*v),
            Value::Uint256(v) => Self::Uint256(v.to_string()),
            Value::Float16(v) => Self::Float64(f32::from(*v) as f64),
            Value::Float32(v) => Self::Float64((*v).into()),
            Value::Float64(v) => Self::Float64(*v),
            Value::Decimal(v) => Self::Decimal(v.to_string()),
            Value::Text(v) => Self::Text(v.clone()),
            Value::Bytes(v) => Self::Bytes(v.clone()),
            Value::Date(v) => Self::Date(*v),
            Value::Time(v) => Self::Time(*v),
            Value::LocalTime(v) => Self::LocalTime(*v),
            Value::DateTime(seconds, nanos) => Self::DateTime {
                seconds: *seconds,
                nanos: *nanos,
            },
            Value::LocalDateTime(seconds, nanos) => Self::LocalDateTime {
                seconds: *seconds,
                nanos: *nanos,
            },
            Value::ZonedDateTime(seconds, nanos, offset_seconds) => Self::ZonedDateTime {
                seconds: *seconds,
                nanos: *nanos,
                offset_seconds: *offset_seconds,
            },
            Value::ZonedTime(nanos, offset_seconds) => Self::ZonedTime {
                nanos: *nanos,
                offset_seconds: *offset_seconds,
            },
            Value::Duration(months, nanos) => Self::Duration {
                months: *months,
                nanos: *nanos,
            },
            Value::List(values) => Self::List(values.iter().map(Self::from).collect()),
            Value::Path(elements) => Self::Path(elements.iter().map(ApiPathElement::from).collect()),
            Value::Record(fields) => Self::Record(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), ApiValue::from(value)))
                    .collect(),
            ),
            Value::Extension(value) => Self::Extension {
                type_name: value.type_name().to_owned(),
                display: value.to_string(),
            },
        }
    }
}

impl From<&ApiValue> for Value {
    fn from(value: &ApiValue) -> Self {
        match value {
            ApiValue::Null => Self::Null,
            ApiValue::Bool(v) => Self::Bool(*v),
            ApiValue::Int64(v) => Self::Int64(*v),
            ApiValue::Uint64(v) => Self::Uint64(*v),
            ApiValue::Int128(v) => Self::Int128(*v),
            ApiValue::Uint128(v) => Self::Uint128(*v),
            ApiValue::Int256(v) => gleaph_gql::types::Int256::parse(v)
                .map(Self::Int256)
                .unwrap_or(Self::Text(v.clone())),
            ApiValue::Uint256(v) => gleaph_gql::types::Uint256::parse(v)
                .map(Self::Uint256)
                .unwrap_or(Self::Text(v.clone())),
            ApiValue::Float64(v) => Self::Float64(*v),
            ApiValue::Decimal(v) => gleaph_gql::types::Decimal::parse(v)
                .map(Self::Decimal)
                .unwrap_or(Self::Text(v.clone())),
            ApiValue::Text(v) => Self::Text(v.clone()),
            ApiValue::Bytes(v) => Self::Bytes(v.clone()),
            ApiValue::Date(v) => Self::Date(*v),
            ApiValue::Time(v) => Self::Time(*v),
            ApiValue::LocalTime(v) => Self::LocalTime(*v),
            ApiValue::DateTime { seconds, nanos } => Self::DateTime(*seconds, *nanos),
            ApiValue::LocalDateTime { seconds, nanos } => Self::LocalDateTime(*seconds, *nanos),
            ApiValue::ZonedDateTime { seconds, nanos, offset_seconds } => {
                Self::ZonedDateTime(*seconds, *nanos, *offset_seconds)
            }
            ApiValue::ZonedTime { nanos, offset_seconds } => Self::ZonedTime(*nanos, *offset_seconds),
            ApiValue::Duration { months, nanos } => Self::Duration(*months, *nanos),
            ApiValue::List(values) => Self::List(values.iter().map(Value::from).collect()),
            ApiValue::Path(elements) => Self::Path(
                elements
                    .iter()
                    .map(|element| match element {
                        ApiPathElement::Vertex(id) => gleaph_gql::types::PathElement::Vertex(*id),
                        ApiPathElement::Edge { src, dst, label } => gleaph_gql::types::PathElement::Edge {
                            src: *src,
                            dst: *dst,
                            label: label.clone(),
                        },
                    })
                    .collect(),
            ),
            ApiValue::Record(fields) => {
                Self::Record(fields.iter().map(|(k, v)| (k.clone(), Value::from(v))).collect())
            }
            ApiValue::Extension { display, .. } => Self::Text(display.clone()),
        }
    }
}

impl From<&PlanSummary> for ApiPlanSummary {
    fn from(value: &PlanSummary) -> Self {
        Self {
            estimated_rows: value.estimated_rows,
            estimated_cost: value.estimated_cost,
            has_dml: value.has_dml,
            dml_error_count: value.dml_error_count,
            dml_warning_count: value.dml_warning_count,
            type_warning_count: value.type_warning_count,
        }
    }
}

impl From<&gleaph_gql_executor::ExecutionSummary> for ApiExecutionSummary {
    fn from(value: &gleaph_gql_executor::ExecutionSummary) -> Self {
        Self {
            row_count: value.row_count,
            warning_count: value.warning_count,
            had_dml: value.had_dml,
        }
    }
}

impl From<&ExecutionResult> for ApiExecutionResult {
    fn from(value: &ExecutionResult) -> Self {
        Self {
            rows: value
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(k, v)| (k.clone(), ApiValue::from(v)))
                        .collect()
                })
                .collect(),
            warnings: value.warnings.clone(),
            summary: ApiExecutionSummary::from(&value.summary),
        }
    }
}

impl From<&PreparedParameterInfo> for ApiPreparedParameterInfo {
    fn from(value: &PreparedParameterInfo) -> Self {
        Self {
            name: value.name.clone(),
            required: value.required,
            nullable: value.nullable,
            inferred: value.inferred,
            type_hints: value.type_hints.clone(),
        }
    }
}

impl From<&PreparedColumnInfo> for ApiPreparedColumnInfo {
    fn from(value: &PreparedColumnInfo) -> Self {
        Self {
            name: value.name.clone(),
            expr: value.expr.clone(),
            aliased: value.aliased,
        }
    }
}

impl From<&PreparedStatementKind> for ApiPreparedStatementKind {
    fn from(value: &PreparedStatementKind) -> Self {
        match value {
            PreparedStatementKind::Query => Self::Query,
            PreparedStatementKind::Mutation => Self::Mutation,
        }
    }
}

impl From<&PreparedStatementInfo> for ApiPreparedStatementInfo {
    fn from(value: &PreparedStatementInfo) -> Self {
        Self {
            name: value.name.clone(),
            kind: ApiPreparedStatementKind::from(&value.kind),
            source: value.source.clone(),
            columns: value.columns.iter().map(ApiPreparedColumnInfo::from).collect(),
            parameters: value
                .parameters
                .iter()
                .map(ApiPreparedParameterInfo::from)
                .collect(),
            type_warnings: value.type_warnings.clone(),
            explain: value.explain.clone(),
            summary: value.summary.clone(),
        }
    }
}

impl From<&gleaph_gql::type_check::TypeDiagnostic> for ApiTypeDiagnostic {
    fn from(value: &gleaph_gql::type_check::TypeDiagnostic) -> Self {
        Self {
            code: value.code.map(str::to_owned),
            message: value.message.clone(),
            span_start: value.span.start as u32,
            span_end: value.span.end as u32,
            severity: match value.severity {
                gleaph_gql::type_check::DiagnosticSeverity::Error => ApiDiagnosticSeverity::Error,
                gleaph_gql::type_check::DiagnosticSeverity::Warning => {
                    ApiDiagnosticSeverity::Warning
                }
            },
        }
    }
}

impl From<&ApiAuthContext> for AuthContext {
    fn from(value: &ApiAuthContext) -> Self {
        Self {
            caller: value.caller.clone(),
            is_controller: value.is_controller,
        }
    }
}

pub(crate) fn normalize_params(params: &BTreeMap<String, Value>) -> BTreeMap<String, Value> {
    let mut normalized = BTreeMap::new();
    for (key, value) in params {
        normalized.insert(key.clone(), value.clone());
        if !key.starts_with('$') {
            normalized.insert(format!("${key}"), value.clone());
        }
    }
    normalized
}

#[derive(Debug, Error)]
pub enum GleaphError {
    #[error(transparent)]
    Parse(#[from] GqlError),
    #[error(transparent)]
    Planner(#[from] PlannerError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error("permission denied for operation `{operation}` (caller={caller:?}, level={level:?})")]
    PermissionDenied {
        operation: String,
        caller: Option<String>,
        level: Option<AccessLevel>,
    },
    #[error("prepared statement `{0}` not found")]
    PreparedNotFound(String),
    #[error("expected a query statement as the first statement in the block")]
    ExpectedQuery,
    #[error("expected a transaction statement block")]
    MissingStatementBlock,
}

pub(crate) fn execute_plan_with_normalized_params<G: GraphRead + GraphWrite>(
    graph: &mut G,
    plan: &gleaph_gql_planner::PhysicalPlan,
    ctx: &ExecutionContext,
) -> Result<ExecutionResult, ExecutionError> {
    execute_plan_with_context(graph, plan, ctx)
}

pub fn plan_query(
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, GleaphError> {
    Ok(build_plan_output(query, stats)?)
}

pub fn parse_query(input: &str) -> Result<LinearQueryStatement, GleaphError> {
    let program = parser::parse(input)?;
    let tx = program
        .transaction_activity
        .ok_or(GleaphError::MissingStatementBlock)?;
    let block = tx.body.ok_or(GleaphError::MissingStatementBlock)?;
    match block.first {
        Statement::Query(composite) => Ok(composite.left),
        _ => Err(GleaphError::ExpectedQuery),
    }
}

pub fn execute_query<G: GraphRead + GraphWrite>(
    graph: &mut G,
    query: &LinearQueryStatement,
    stats: Option<&dyn GraphStats>,
    ctx: &ExecutionContext,
) -> Result<QueryRunOutput, GleaphError> {
    let plan = build_plan_output(query, stats)?;
    let execution = execute_plan_with_context(graph, &plan.plan, ctx)?;
    Ok(QueryRunOutput { plan, execution })
}

pub fn plan_query_str(
    input: &str,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, GleaphError> {
    let query = parse_query(input)?;
    plan_query(&query, stats)
}

pub fn execute_query_str<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: &str,
    stats: Option<&dyn GraphStats>,
    ctx: &ExecutionContext,
) -> Result<QueryRunOutput, GleaphError> {
    let query = parse_query(input)?;
    execute_query(graph, &query, stats, ctx)
}

pub fn plan_request(
    request: &QueryRequest,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanResponse, GleaphError> {
    let plan = plan_query_str(&request.query, stats)?;
    Ok(PlanResponse {
        explain: plan.explain,
        summary: plan.summary,
    })
}

pub fn plan_api_request(
    request: &ApiQueryRequest,
    stats: Option<&dyn GraphStats>,
) -> Result<ApiPlanResponse, GleaphError> {
    let request = QueryRequest {
        query: request.query.clone(),
        params: request
            .params
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v)))
            .collect(),
    };
    let response = plan_request(&request, stats)?;
    Ok(ApiPlanResponse {
        explain: response.explain,
        summary: ApiPlanSummary::from(&response.summary),
    })
}

pub fn execute_request<G: GraphRead + GraphWrite>(
    graph: &mut G,
    request: &QueryRequest,
    stats: Option<&dyn GraphStats>,
) -> Result<QueryResponse, GleaphError> {
    let ctx = ExecutionContext {
        params: normalize_params(&request.params),
    };
    let output = execute_query_str(graph, &request.query, stats, &ctx)?;
    Ok(QueryResponse {
        explain: output.plan.explain,
        plan_summary: output.plan.summary,
        execution: output.execution,
    })
}

pub fn execute_api_request<G: GraphRead + GraphWrite>(
    graph: &mut G,
    request: &ApiQueryRequest,
    stats: Option<&dyn GraphStats>,
) -> Result<ApiQueryResponse, GleaphError> {
    let request = QueryRequest {
        query: request.query.clone(),
        params: request
            .params
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v)))
            .collect(),
    };
    let response = execute_request(graph, &request, stats)?;
    Ok(ApiQueryResponse {
        explain: response.explain,
        plan_summary: ApiPlanSummary::from(&response.plan_summary),
        execution: ApiExecutionResult::from(&response.execution),
    })
}

pub fn plan_block(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, GleaphError> {
    Ok(build_block_plan_output(block, stats)?)
}

pub fn parse_block(input: &str) -> Result<StatementBlock, GleaphError> {
    let program = parser::parse(input)?;
    let tx = program
        .transaction_activity
        .ok_or(GleaphError::MissingStatementBlock)?;
    tx.body.ok_or(GleaphError::MissingStatementBlock)
}

pub fn execute_block<G: GraphRead + GraphWrite>(
    graph: &mut G,
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
    ctx: &ExecutionContext,
) -> Result<QueryRunOutput, GleaphError> {
    let plan = build_block_plan_output(block, stats)?;
    let execution = execute_plan_with_context(graph, &plan.plan, ctx)?;
    Ok(QueryRunOutput { plan, execution })
}

pub fn plan_block_str(
    input: &str,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, GleaphError> {
    let block = parse_block(input)?;
    plan_block(&block, stats)
}

pub fn execute_block_str<G: GraphRead + GraphWrite>(
    graph: &mut G,
    input: &str,
    stats: Option<&dyn GraphStats>,
    ctx: &ExecutionContext,
) -> Result<QueryRunOutput, GleaphError> {
    let block = parse_block(input)?;
    execute_block(graph, &block, stats, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepared::PreparedStatementKind;
    use gleaph_graph_mem::InMemoryGraph;
    use serde_json::{from_str, to_string};

    #[test]
    fn execute_query_returns_plan_and_execution_dtos() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);

        let query = parse_query("MATCH (n:User) RETURN n.name").expect("parse query");
        let output =
            execute_query(&mut graph, &query, None, &ExecutionContext::default()).expect("run");

        assert_eq!(output.execution.rows.len(), 1);
        assert_eq!(output.execution.summary.row_count, 1);
        assert!(!output.plan.summary.has_dml);
        assert!(output.plan.explain.contains("Plan:"));
    }

    #[test]
    fn execute_block_surfaces_dml_summary() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let block = parse_block("MATCH (n:User) SET n.name = 'updated' RETURN n").expect("parse block");
        let output =
            execute_block(&mut graph, &block, None, &ExecutionContext::default()).expect("run");

        assert!(output.plan.summary.has_dml);
        assert!(output.execution.summary.had_dml);
        assert!(output.plan.explain.contains("Data modification: yes"));
    }

    #[test]
    fn execute_query_str_parses_and_runs() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("Alice".to_owned()))]);

        let output = execute_query_str(
            &mut graph,
            "MATCH (n:User) RETURN n.name",
            None,
            &ExecutionContext::default(),
        )
        .expect("run");

        assert_eq!(output.execution.summary.row_count, 1);
        assert!(output.plan.explain.contains("NodeScan"));
    }

    #[test]
    fn plan_query_str_rejects_non_query_first_statement() {
        let err = plan_query_str("INSERT (n:User)", None).expect_err("should reject non-query");
        assert!(matches!(err, GleaphError::ExpectedQuery));
    }

    #[test]
    fn plan_request_returns_api_friendly_response() {
        let request = QueryRequest {
            query: "MATCH (n:User) RETURN n.name".to_owned(),
            params: BTreeMap::new(),
        };

        let response = plan_request(&request, None).expect("plan request");
        assert!(response.explain.contains("Plan:"));
        assert!(!response.summary.has_dml);
    }

    #[test]
    fn execute_request_uses_params_and_returns_response_dto() {
        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let request = QueryRequest {
            query: "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid".to_owned(),
            params: [("uid".to_owned(), Value::Text("u1".to_owned()))]
                .into_iter()
                .collect(),
        };

        let response = execute_request(&mut graph, &request, None).expect("execute request");
        assert_eq!(response.execution.summary.row_count, 1);
        assert!(!response.plan_summary.has_dml);
        assert!(response.explain.contains("Plan:"));
    }

    #[test]
    fn api_request_and_response_are_json_serializable() {
        let request = ApiQueryRequest {
            query: "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid".to_owned(),
            params: [("uid".to_owned(), ApiValue::Text("u1".to_owned()))]
                .into_iter()
                .collect(),
        };

        let json = to_string(&request).expect("serialize request");
        let decoded: ApiQueryRequest = from_str(&json).expect("deserialize request");
        assert_eq!(decoded, request);

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let response = execute_api_request(&mut graph, &request, None).expect("execute api request");
        let json = to_string(&response).expect("serialize response");
        let decoded: ApiQueryResponse = from_str(&json).expect("deserialize response");
        assert_eq!(decoded.execution.summary.row_count, 1);
        assert_eq!(decoded.plan_summary.has_dml, false);
    }

    #[test]
    fn anonymous_can_execute_prepared_query_but_not_query_or_prepare() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller("controller");
        let anonymous = AuthContext::anonymous();
        service
            .prepare(&admin, "users_by_uid", "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid", None)
            .expect("prepare");

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let params = [("uid".to_owned(), Value::Text("u1".to_owned()))]
            .into_iter()
            .collect();
        let response = service
            .execute_prepared(&mut graph, &anonymous, "users_by_uid", &params)
            .expect("anonymous prepared execute");
        assert_eq!(response.execution.summary.row_count, 1);

        let query_request = QueryRequest {
            query: "MATCH (n:User) RETURN n.uid".to_owned(),
            params: BTreeMap::new(),
        };
        let err = service
            .execute_request(&mut graph, &anonymous, &query_request, None)
            .expect_err("anonymous direct query should be denied");
        assert!(matches!(err, GleaphError::PermissionDenied { .. }));

        let err = service
            .prepare(&anonymous, "denied", "MATCH (n) RETURN n", None)
            .expect_err("anonymous prepare should be denied");
        assert!(matches!(err, GleaphError::PermissionDenied { .. }));
    }

    #[test]
    fn anonymous_can_execute_prepared_mutation_only() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller("controller");
        let anonymous = AuthContext::anonymous();
        let prepared = service
            .prepare(&admin, "set_user_name", "MATCH (n:User) SET n.name = 'updated' RETURN n", None)
            .expect("prepare");
        assert_eq!(prepared.kind, PreparedStatementKind::Mutation);

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("name", Value::Text("before".to_owned()))]);

        let response = service
            .execute_prepared(&mut graph, &anonymous, "set_user_name", &BTreeMap::new())
            .expect("anonymous prepared mutation");
        assert!(response.execution.summary.had_dml);
    }

    #[test]
    fn read_user_can_query_and_list_prepared_but_not_prepare() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller("controller");
        let reader = AuthContext::principal("reader");
        service
            .set_acl_entry(&admin, "reader", AccessLevel::Read)
            .expect("set acl");
        service
            .prepare(&admin, "users", "MATCH (n:User) RETURN n", None)
            .expect("prepare");

        let listed = service.list_prepared(&reader).expect("reader list prepared");
        assert_eq!(listed.len(), 1);

        let err = service
            .prepare(&reader, "denied", "MATCH (n) RETURN n", None)
            .expect_err("reader prepare should be denied");
        assert!(matches!(err, GleaphError::PermissionDenied { .. }));
    }

    #[test]
    fn prepared_info_exposes_parameter_metadata_and_type_warnings() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller("controller");
        let prepared = service
            .prepare(
                &admin,
                "warn_query",
                "MATCH (n:User) WHERE n.uid = $uid RETURN abs('oops')",
                None,
            )
            .expect("prepare");

        assert_eq!(prepared.parameters.len(), 1);
        assert_eq!(prepared.parameters[0].name, "uid");
        assert!(prepared.parameters[0].required);
        assert!(prepared.parameters[0].inferred);
        assert!(!prepared.type_warnings.is_empty());
    }

    #[test]
    fn prepared_api_dtos_are_serializable_and_executable() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller("controller");
        let anonymous = AuthContext::anonymous();

        let prepare_request = ApiPrepareRequest {
            name: "users_by_uid".to_owned(),
            query: "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid AS uid".to_owned(),
        };
        let prepared = service
            .prepare_api(&admin, &prepare_request, None)
            .expect("prepare api");
        assert_eq!(prepared.prepared.name, "users_by_uid");
        assert_eq!(prepared.prepared.columns[0].name, "uid");

        let json = to_string(&prepare_request).expect("serialize prepare request");
        let decoded: ApiPrepareRequest = from_str(&json).expect("deserialize prepare request");
        assert_eq!(decoded, prepare_request);

        let listed = service
            .list_prepared_api(&admin)
            .expect("list prepared api");
        assert_eq!(listed.statements.len(), 1);

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let execute_request = ApiExecutePreparedRequest {
            name: "users_by_uid".to_owned(),
            params: [("uid".to_owned(), ApiValue::Text("u1".to_owned()))]
                .into_iter()
                .collect(),
        };
        let response = service
            .execute_prepared_api_request(&mut graph, &anonymous, &execute_request)
            .expect("execute prepared api request");
        assert_eq!(response.execution.summary.row_count, 1);

        let drop_request = ApiDropPreparedRequest {
            name: "users_by_uid".to_owned(),
        };
        let dropped = service
            .drop_prepared_api(&admin, &drop_request)
            .expect("drop prepared api");
        assert!(dropped.dropped);
    }

    #[test]
    fn auth_wrapped_endpoint_dtos_round_trip_and_execute() {
        let mut service = GleaphService::new();
        let admin_endpoint = ApiAuthContext {
            caller: Some("controller".to_owned()),
            is_controller: true,
        };
        let anonymous_endpoint = ApiAuthContext::default();

        let prepare_endpoint = ApiPrepareEndpointRequest {
            auth: admin_endpoint.clone(),
            request: ApiPrepareRequest {
                name: "public_users".to_owned(),
                query: "MATCH (n:User) RETURN n.uid AS uid".to_owned(),
            },
        };
        let prepared = service
            .prepare_api_endpoint(&prepare_endpoint, None)
            .expect("prepare endpoint");
        assert_eq!(prepared.prepared.name, "public_users");

        let json = to_string(&prepare_endpoint).expect("serialize endpoint request");
        let decoded: ApiPrepareEndpointRequest =
            from_str(&json).expect("deserialize endpoint request");
        assert_eq!(decoded, prepare_endpoint);

        let list_endpoint = ApiListPreparedEndpointRequest {
            auth: admin_endpoint.clone(),
        };
        let listed = service
            .list_prepared_api_endpoint(&list_endpoint)
            .expect("list endpoint");
        assert_eq!(listed.statements.len(), 1);

        let mut graph = InMemoryGraph::new();
        graph.insert_node(["User"], [("uid", Value::Text("u1".to_owned()))]);

        let execute_endpoint = ApiExecutePreparedEndpointRequest {
            auth: anonymous_endpoint,
            request: ApiExecutePreparedRequest {
                name: "public_users".to_owned(),
                params: BTreeMap::new(),
            },
        };
        let response = service
            .execute_prepared_api_endpoint(&mut graph, &execute_endpoint)
            .expect("execute endpoint");
        assert_eq!(response.execution.summary.row_count, 1);
    }
}
