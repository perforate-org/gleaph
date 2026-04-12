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
//! - IC principals are serialized as `ApiValue::Principal`
//! - other extension values fall back to their display text
//!
//! The reverse conversion accepts those encodings and falls back to
//! `Value::Text` when a string-encoded wide numeric value cannot be parsed.

mod auth;
mod canister_host;
pub mod catalog;
mod graph_registry;
mod prepared;
mod procedure;
mod service;
mod subplan_wire_v1;

#[cfg(feature = "canbench-rs")]
mod bench;

#[cfg(feature = "canbench-rs")]
mod canbench_benches {
    use canbench_rs::bench;

    #[bench(raw)]
    fn bench_gql_execute_block_bulk_detach_delete() -> canbench_rs::BenchResult {
        super::bench::bench_gql_execute_block_bulk_detach_delete_impl()
    }
}

pub use subplan_wire_v1::SubplanWireV1;

use candid::CandidType;
use candid::Principal as CandidPrincipal;
use gleaph_gql::Value;
use gleaph_gql::ast::{LinearQueryStatement, Statement, StatementBlock};
use gleaph_gql::{GqlError, parser};
use gleaph_gql_executor::{
    ExecutionContext, ExecutionError, ExecutionResult, execute_plan_with_context,
};
use gleaph_gql_planner::{
    GraphStats, PhysicalPlan, PlanBuildOutput, PlanSummary, PlannerError, build_plan_output,
    build_plan_output_for_execute, first_executor_unsupported_op,
};
use gleaph_graph_kernel::{GraphRead, GraphWrite};
use gleaph_graph_store::integration::GraphStoreKernelOverlay;
use ic_cdk::export_candid;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};
use serde::{Deserialize, Serialize};
#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
#[cfg(target_arch = "wasm32")]
use std::time::Duration;
use thiserror::Error;

pub use auth::{AccessLevel, AclEntry, AuthContext, Operation, PermissionChecker, Principal};
pub use gleaph_graph_kernel::{GraphError, GraphErrorKind};
pub use graph_registry::{
    IcGraphRegistryResolver, IcUseGraphRouter, InMemoryGraphRegistryResolver,
};
pub use prepared::{
    PreparedColumnInfo, PreparedOptions, PreparedParameterInfo, PreparedQueryInfo,
    PreparedQueryKind, PreparedQueryRegistry, PreparedSortKey, PreparedSortSpec,
};
pub use procedure::{
    DelegatingProcedureRegistry, delegated_procedure_registry, standard_procedure_registry,
};
pub use service::{GleaphService, GleaphServiceCoreSnapshot};

thread_local! {
    #[cfg(target_arch = "wasm32")]
    static VACUUM_TIMER: RefCell<Option<ic_cdk_timers::TimerId>> = RefCell::new(None);
    #[cfg(target_arch = "wasm32")]
    static VACUUM_TIMER_INTERVAL_SECS: RefCell<u64> = const { RefCell::new(PERIODIC_VACUUM_INTERVAL_SECS) };
    #[cfg(target_arch = "wasm32")]
    static VACUUM_BACKLOG_PROVIDER: RefCell<Option<Arc<dyn Fn() -> usize>>> = const { RefCell::new(None) };
    #[cfg(target_arch = "wasm32")]
    static VACUUM_TICK_HANDLER: RefCell<Option<Arc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Runs graph flush after `f`. Persists the service stable cell only when `f` returns `true`
/// (graph catalog changed — see [`GleaphService::execute_api_request_with_service_stable_dirty`]).
fn with_canister_graph_and_service<R>(
    f: impl for<'a> FnOnce(
        &mut GleaphService,
        &mut GraphStoreKernelOverlay<'a, canister_host::CanisterGraphMemory>,
    ) -> (R, bool),
) -> R {
    canister_host::CanisterHost::ensure_installed();
    canister_host::CanisterHost::with(|host| {
        let (out, persist_service) = host.with_graph_overlay_and_service(f);
        host.flush_graph_stable_full();
        if persist_service {
            host.persist_service_stable();
        }
        out
    })
}

#[cfg(test)]
fn with_canister_graph<R>(
    f: impl for<'a> FnOnce(&mut GraphStoreKernelOverlay<'a, canister_host::CanisterGraphMemory>) -> R,
) -> R {
    with_canister_graph_and_service(|_service, overlay| (f(overlay), false))
}

/// Graph flush to stable (region manager cell + dirty pages) without rewriting the service cell.
/// Used by periodic vacuum so maintenance work does not re-encode the full service snapshot each tick.
#[cfg(target_arch = "wasm32")]
fn with_canister_graph_maintenance<R>(
    f: impl for<'a> FnOnce(&mut GraphStoreKernelOverlay<'a, canister_host::CanisterGraphMemory>) -> R,
) -> R {
    canister_host::CanisterHost::ensure_installed();
    canister_host::CanisterHost::with(|host| {
        let mut overlay = host.bind_graph_overlay();
        let out = f(&mut overlay);
        host.flush_graph_stable_full();
        out
    })
}

fn with_canister_service<R>(f: impl FnOnce(&GleaphService) -> R) -> R {
    canister_host::CanisterHost::ensure_installed();
    canister_host::CanisterHost::with(|host| f(&host.service))
}

fn with_canister_service_mut_persist<R>(f: impl FnOnce(&mut GleaphService) -> R) -> R {
    canister_host::CanisterHost::ensure_installed();
    canister_host::CanisterHost::with(|host| {
        let out = f(&mut host.service);
        host.persist_service_stable();
        out
    })
}

#[cfg(target_arch = "wasm32")]
const PERIODIC_VACUUM_INTERVAL_SECS: u64 = 60;
#[cfg(target_arch = "wasm32")]
const VACUUM_INTERVAL_BUSY_SECS: u64 = 15;
#[cfg(target_arch = "wasm32")]
const VACUUM_INTERVAL_IDLE_SECS: u64 = 300;
#[cfg(target_arch = "wasm32")]
const VACUUM_QUEUE_BUSY_THRESHOLD: usize = 100;

fn default_procedure_registry() -> Arc<dyn gleaph_gql_executor::ProcedureRegistry> {
    standard_procedure_registry()
}

#[derive(Clone, Debug)]
pub struct QueryRunOutput {
    /// [`PlanBuildOutput::explain`] is empty when produced by [`execute_query`] / [`execute_block`]
    /// (see [`build_plan_output_for_execute`]). Use [`plan_query`] / [`plan_block`] for explain text.
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
    pub use_graph_pushdown: Vec<ApiUseGraphPushdownInfo>,
}

/// Query execution response for internal Rust callers.
#[derive(Clone, Debug)]
pub struct QueryResponse {
    /// Always empty on successful execute: explain text is omitted on the execute hot path.
    /// Call [`plan_request`] / [`plan_query_str`] (or inspect [`QueryRunOutput::plan`] via
    /// [`plan_query`]) when you need human-readable plan output.
    pub explain: String,
    pub plan_summary: PlanSummary,
    pub use_graph_pushdown: Vec<ApiUseGraphPushdownInfo>,
    pub execution: ExecutionResult,
}

/// Stable serialized value type for external API boundaries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
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
    DateTime {
        seconds: i64,
        nanos: u32,
    },
    LocalDateTime {
        seconds: i64,
        nanos: u32,
    },
    ZonedDateTime {
        seconds: i64,
        nanos: u32,
        offset_seconds: i32,
    },
    ZonedTime {
        nanos: u64,
        offset_seconds: i32,
    },
    Duration {
        months: i32,
        nanos: i64,
    },
    Principal(CandidPrincipal),
    List(Vec<ApiValue>),
    Path(Vec<ApiPathElement>),
    Record(BTreeMap<String, ApiValue>),
}

/// Serialized representation of a path element.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum ApiPathElement {
    Vertex(u64),
    Edge {
        src: u64,
        dst: u64,
        label: Option<String>,
    },
}

/// External query request DTO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiQueryRequest {
    pub query: String,
    pub params: BTreeMap<String, ApiValue>,
}

/// Wire-format auth context: `caller` is the textual encoding of an IC [`Principal`].
///
/// Invalid text is dropped when converting to [`AuthContext`] (caller becomes `None`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiAuthContext {
    pub caller: Option<String>,
    pub is_controller: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiPreparedParameterInfo {
    pub name: String,
    pub required: bool,
    pub nullable: bool,
    pub inferred: bool,
    pub type_hints: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiPreparedColumnInfo {
    pub name: String,
    pub expr: String,
    pub aliased: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ApiPreparedQueryKind {
    Query,
    Update,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiPreparedQueryInfo {
    pub name: String,
    pub kind: ApiPreparedQueryKind,
    pub requires_caller: bool,
    pub extension_types: Vec<String>,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub columns: Vec<ApiPreparedColumnInfo>,
    pub parameters: Vec<ApiPreparedParameterInfo>,
    #[serde(default)]
    pub allowed_sorts: Vec<PreparedSortKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sort: Option<Vec<PreparedSortSpec>>,
    pub type_warnings: Vec<ApiTypeDiagnostic>,
    pub explain: String,
    pub summary: ApiPlanSummary,
    #[serde(default)]
    pub use_graph_pushdown: Vec<ApiUseGraphPushdownInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiPrepareRequest {
    pub name: String,
    pub query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<PreparedOptions>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiPrepareResponse {
    pub prepared: ApiPreparedQueryInfo,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiExecutePreparedRequest {
    pub name: String,
    pub params: BTreeMap<String, ApiValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<Vec<PreparedSortSpec>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiDropPreparedRequest {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiDropPreparedResponse {
    pub dropped: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiListPreparedResponse {
    pub statements: Vec<ApiPreparedQueryInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiPlanEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiQueryRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiExecuteEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiQueryRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiPrepareEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiPrepareRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiExecutePreparedEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiExecutePreparedRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiDropPreparedEndpointRequest {
    pub auth: ApiAuthContext,
    pub request: ApiDropPreparedRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiListPreparedEndpointRequest {
    pub auth: ApiAuthContext,
}

/// Serialized planner summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiPlanSummary {
    pub estimated_rows: Option<f64>,
    pub estimated_cost: Option<f64>,
    pub has_dml: bool,
    pub dml_error_count: usize,
    pub dml_warning_count: usize,
    pub type_warning_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ApiDiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiTypeDiagnostic {
    pub code: Option<String>,
    pub message: String,
    pub span_start: u32,
    pub span_end: u32,
    pub severity: ApiDiagnosticSeverity,
}

/// Serialized execution summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiExecutionSummary {
    pub row_count: usize,
    pub warning_count: usize,
    pub had_dml: bool,
}

/// Serialized execution result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiExecutionResult {
    pub rows: Vec<BTreeMap<String, ApiValue>>,
    pub warnings: Vec<String>,
    pub summary: ApiExecutionSummary,
}

/// External planner response DTO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiPlanResponse {
    pub explain: String,
    pub summary: ApiPlanSummary,
    #[serde(default)]
    pub use_graph_pushdown: Vec<ApiUseGraphPushdownInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ApiUseGraphPushdownInfo {
    pub graph_name: String,
    pub supported: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

const USE_GRAPH_PUSHDOWN_WARNING_PREFIX: &str = "remote USE GRAPH pushdown unavailable";

/// Max parameter rows for [`execute_routed_query_batch`] (federated `USE GRAPH` batching).
pub const MAX_FEDERATION_ROUTED_PARAM_ROWS: usize = 512;

/// External query response DTO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct ApiQueryResponse {
    pub explain: String,
    pub plan_summary: ApiPlanSummary,
    #[serde(default)]
    pub use_graph_pushdown: Vec<ApiUseGraphPushdownInfo>,
    pub execution: ApiExecutionResult,
}

impl ApiUseGraphPushdownInfo {
    pub fn is_unsupported(&self) -> bool {
        !self.supported
    }
}

fn collect_unsupported_use_graph_pushdowns(
    infos: &[ApiUseGraphPushdownInfo],
) -> Vec<ApiUseGraphPushdownInfo> {
    infos
        .iter()
        .filter(|info| info.is_unsupported())
        .cloned()
        .collect()
}

fn collect_use_graph_pushdown_warnings(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .filter(|warning| warning.starts_with(USE_GRAPH_PUSHDOWN_WARNING_PREFIX))
        .cloned()
        .collect()
}

impl ApiPreparedQueryInfo {
    pub fn unsupported_use_graph_pushdowns(&self) -> Vec<ApiUseGraphPushdownInfo> {
        collect_unsupported_use_graph_pushdowns(&self.use_graph_pushdown)
    }
}

impl ApiPlanResponse {
    pub fn unsupported_use_graph_pushdowns(&self) -> Vec<ApiUseGraphPushdownInfo> {
        collect_unsupported_use_graph_pushdowns(&self.use_graph_pushdown)
    }
}

impl ApiQueryResponse {
    pub fn unsupported_use_graph_pushdowns(&self) -> Vec<ApiUseGraphPushdownInfo> {
        collect_unsupported_use_graph_pushdowns(&self.use_graph_pushdown)
    }

    pub fn use_graph_pushdown_warnings(&self) -> Vec<String> {
        collect_use_graph_pushdown_warnings(&self.execution.warnings)
    }
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
            Value::Float128(v) => Self::Float64(*v as f64),
            Value::Float256(v) => Self::Float64(v.to_string().parse::<f64>().unwrap_or(f64::NAN)),
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
            Value::Extension(value) => match value
                .as_any()
                .downcast_ref::<gleaph_gql_ic::PrincipalValue>()
            {
                Some(principal) => Self::Principal(principal.0),
                None => Self::Text(value.to_string()),
            },
            Value::List(values) => Self::List(values.iter().map(Self::from).collect()),
            Value::Path(elements) => {
                Self::Path(elements.iter().map(ApiPathElement::from).collect())
            }
            Value::Record(fields) => Self::Record(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), ApiValue::from(value)))
                    .collect(),
            ),
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
            ApiValue::ZonedDateTime {
                seconds,
                nanos,
                offset_seconds,
            } => Self::ZonedDateTime(*seconds, *nanos, *offset_seconds),
            ApiValue::ZonedTime {
                nanos,
                offset_seconds,
            } => Self::ZonedTime(*nanos, *offset_seconds),
            ApiValue::Duration { months, nanos } => Self::Duration(*months, *nanos),
            ApiValue::Principal(principal) => {
                Self::Extension(Box::new(gleaph_gql_ic::PrincipalValue(*principal)))
            }
            ApiValue::List(values) => Self::List(values.iter().map(Value::from).collect()),
            ApiValue::Path(elements) => Self::Path(
                elements
                    .iter()
                    .map(|element| match element {
                        ApiPathElement::Vertex(id) => gleaph_gql::types::PathElement::Vertex(*id),
                        ApiPathElement::Edge { src, dst, label } => {
                            gleaph_gql::types::PathElement::Edge {
                                src: *src,
                                dst: *dst,
                                label: label.clone(),
                            }
                        }
                    })
                    .collect(),
            ),
            ApiValue::Record(fields) => Self::Record(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(v)))
                    .collect(),
            ),
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

impl From<&PreparedQueryKind> for ApiPreparedQueryKind {
    fn from(value: &PreparedQueryKind) -> Self {
        match value {
            PreparedQueryKind::Query => Self::Query,
            PreparedQueryKind::Update => Self::Update,
        }
    }
}

impl From<&PreparedQueryInfo> for ApiPreparedQueryInfo {
    fn from(value: &PreparedQueryInfo) -> Self {
        Self {
            name: value.name.clone(),
            kind: ApiPreparedQueryKind::from(&value.kind),
            requires_caller: value.requires_caller,
            extension_types: value.extension_types.clone(),
            source: value.source.clone(),
            description: value.description.clone(),
            columns: value
                .columns
                .iter()
                .map(ApiPreparedColumnInfo::from)
                .collect(),
            parameters: value
                .parameters
                .iter()
                .map(ApiPreparedParameterInfo::from)
                .collect(),
            allowed_sorts: value.allowed_sorts.clone(),
            default_sort: value.default_sort.clone(),
            type_warnings: value.type_warnings.clone(),
            explain: value.explain.clone(),
            summary: value.summary.clone(),
            use_graph_pushdown: value.use_graph_pushdown.clone(),
        }
    }
}

impl From<&gleaph_gql_planner::UseGraphPushdownInfo> for ApiUseGraphPushdownInfo {
    fn from(value: &gleaph_gql_planner::UseGraphPushdownInfo) -> Self {
        Self {
            graph_name: value.graph_name.clone(),
            supported: value.supported,
            reason: value.reason.clone(),
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
            caller: value
                .caller
                .as_ref()
                .and_then(|text| Principal::from_text(text).ok()),
            is_controller: value.is_controller,
            query_subject: None,
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
    #[error("prepared statement: {0}")]
    PreparedValidation(String),
    #[error(transparent)]
    Parse(#[from] GqlError),
    #[error(transparent)]
    Planner(#[from] PlannerError),
    #[error(transparent)]
    Execution(#[from] ExecutionError),
    #[error("permission denied for operation `{operation}` (caller={caller:?}, level={level:?})")]
    PermissionDenied {
        operation: String,
        caller: Option<Principal>,
        level: Option<AccessLevel>,
    },
    #[error("prepared query `{0}` not found")]
    PreparedNotFound(String),
    #[error("unsupported extension type `{0}`")]
    UnsupportedExtensionType(String),
    #[error("expected a query statement as the first statement in the block")]
    ExpectedQuery,
    #[error("expected a transaction statement block")]
    MissingStatementBlock,
    #[error("federated routed query: {0}")]
    FederationRoutedQuery(String),
    #[error("graph catalog: {0}")]
    Catalog(String),
}

impl From<catalog::PlanBlockError> for GleaphError {
    fn from(e: catalog::PlanBlockError) -> Self {
        match e {
            catalog::PlanBlockError::Catalog(c) => GleaphError::Catalog(c.to_string()),
            catalog::PlanBlockError::Planner(p) => GleaphError::Planner(p),
        }
    }
}

impl From<catalog::CatalogError> for GleaphError {
    fn from(e: catalog::CatalogError) -> Self {
        GleaphError::Catalog(e.to_string())
    }
}

impl GleaphError {
    /// Returns the underlying [`GraphError`] when this is an execution error wrapping the graph.
    pub fn as_graph_error(&self) -> Option<&GraphError> {
        match self {
            GleaphError::Execution(e) => e.as_graph_error(),
            _ => None,
        }
    }

    /// [`GraphError::kind`] when [`Self::as_graph_error`] is [`Some`].
    pub fn graph_error_kind(&self) -> Option<GraphErrorKind> {
        self.as_graph_error().map(GraphError::kind)
    }
}

pub(crate) fn ensure_plan_supported_by_executor(plan: &PhysicalPlan) -> Result<(), GleaphError> {
    if let Some(op) = first_executor_unsupported_op(plan) {
        return Err(GleaphError::Execution(ExecutionError::UnsupportedPlanOp(
            op,
        )));
    }
    Ok(())
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
    let out = build_plan_output(query, stats)?;
    ensure_plan_supported_by_executor(&out.plan)?;
    Ok(out)
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
    let plan = {
        #[cfg(feature = "canbench-rs")]
        let _scope = canbench_rs::bench_scope("gql_query_plan");
        build_plan_output_for_execute(query, stats)?
    };
    ensure_plan_supported_by_executor(&plan.plan)?;
    let execution = {
        #[cfg(feature = "canbench-rs")]
        let _scope = canbench_rs::bench_scope("gql_query_execute");
        execute_plan_with_context(graph, &plan.plan, ctx)?
    };
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
    let query = {
        #[cfg(feature = "canbench-rs")]
        let _scope = canbench_rs::bench_scope("gql_query_parse");
        parse_query(input)?
    };
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
        use_graph_pushdown: plan
            .plan
            .annotations
            .optimizer
            .use_graph_pushdown
            .iter()
            .map(ApiUseGraphPushdownInfo::from)
            .collect(),
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
        use_graph_pushdown: response.use_graph_pushdown,
    })
}

pub fn execute_request<G: GraphRead + GraphWrite>(
    graph: &mut G,
    request: &QueryRequest,
    stats: Option<&dyn GraphStats>,
) -> Result<QueryResponse, GleaphError> {
    let ctx = ExecutionContext {
        params: normalize_params(&request.params),
        caller: None,
        procedure_registry: Some(default_procedure_registry()),
        ..ExecutionContext::default()
    };
    let output = execute_query_str(graph, &request.query, stats, &ctx)?;
    Ok(QueryResponse {
        explain: output.plan.explain,
        plan_summary: output.plan.summary,
        use_graph_pushdown: output
            .plan
            .plan
            .use_graph_pushdown()
            .iter()
            .map(ApiUseGraphPushdownInfo::from)
            .collect(),
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
        use_graph_pushdown: response.use_graph_pushdown,
        execution: ApiExecutionResult::from(&response.execution),
    })
}

pub fn plan_block(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, GleaphError> {
    plan_block_with_catalog(block, stats, &catalog::GraphCatalog::default(), None)
}

/// Like [`plan_block`], but uses `catalog` and `active_graph` to supply a [`PropertySchema`] for planning.
pub fn plan_block_with_catalog(
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
    catalog: &catalog::GraphCatalog,
    active_graph: Option<&str>,
) -> Result<PlanBuildOutput, GleaphError> {
    let out = catalog::plan_block_with_catalog(block, stats, catalog, active_graph)
        .map_err(GleaphError::from)?;
    ensure_plan_supported_by_executor(&out.plan)?;
    Ok(out)
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
    execute_block_with_catalog(graph, block, stats, ctx, &catalog::GraphCatalog::default())
}

/// Like [`execute_block`], but supplies a [`PropertySchema`] from `catalog` using `ctx.selected_graph`.
pub fn execute_block_with_catalog<G: GraphRead + GraphWrite>(
    graph: &mut G,
    block: &StatementBlock,
    stats: Option<&dyn GraphStats>,
    ctx: &ExecutionContext,
    catalog: &catalog::GraphCatalog,
) -> Result<QueryRunOutput, GleaphError> {
    #[cfg(feature = "canbench-rs")]
    let _scope = canbench_rs::bench_scope("gql_block_plan");
    catalog::execute_block_with_catalog(graph, block, stats, ctx, catalog)
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
    let block = {
        #[cfg(feature = "canbench-rs")]
        let _scope = canbench_rs::bench_scope("gql_block_parse");
        parse_block(input)?
    };
    execute_block(graph, &block, stats, ctx)
}

fn canister_auth_context() -> AuthContext {
    let caller = ic_cdk::api::msg_caller();
    let is_controller = ic_cdk::api::is_controller(&caller);
    AuthContext {
        caller: Some(caller),
        is_controller,
        query_subject: None,
    }
}

fn map_err(err: GleaphError) -> String {
    err.to_string()
}

/// Registers a runtime provider for GC backlog length (`reclaim_queue` size).
///
/// This allows the canister wiring layer to connect `graph-store` vacuum stats
/// without hard-coupling this crate to one concrete runtime backend.
#[cfg(target_arch = "wasm32")]
pub fn set_vacuum_backlog_provider(provider: Option<Arc<dyn Fn() -> usize>>) {
    VACUUM_BACKLOG_PROVIDER.with(|slot| {
        *slot.borrow_mut() = provider;
    });
    maybe_reschedule_periodic_vacuum_timer();
}

/// Registers a periodic vacuum handler invoked on every timer tick.
///
/// Typical use: call graph-store `vacuum_step(max_ops)` and then expose queue
/// length via [`set_vacuum_backlog_provider`].
#[cfg(target_arch = "wasm32")]
pub fn set_vacuum_tick_handler(handler: Option<Arc<dyn Fn()>>) {
    VACUUM_TICK_HANDLER.with(|slot| {
        *slot.borrow_mut() = handler;
    });
}

/// Convenience helper to wire periodic vacuum runtime hooks at once.
///
/// - `backlog_provider`: returns current reclaim queue length.
/// - `tick_handler`: runs one bounded maintenance step (e.g. `vacuum_step`).
#[cfg(target_arch = "wasm32")]
pub fn wire_vacuum_runtime_hooks(
    backlog_provider: Option<Arc<dyn Fn() -> usize>>,
    tick_handler: Option<Arc<dyn Fn()>>,
) {
    set_vacuum_tick_handler(tick_handler);
    set_vacuum_backlog_provider(backlog_provider);
}

#[cfg(target_arch = "wasm32")]
async fn run_periodic_maintenance_tick() {
    VACUUM_TICK_HANDLER.with(|handler| {
        if let Some(run) = handler.borrow().as_ref() {
            run();
        }
    });
    // The current canister runtime uses in-memory graph state by default.
    // Keep this hook explicit so graph-store vacuum wiring can be attached here.
    maybe_reschedule_periodic_vacuum_timer();
}

#[cfg(target_arch = "wasm32")]
const MAX_OPS_PER_VACUUM_TICK: usize = 64;

fn wire_periodic_vacuum_runtime() {
    #[cfg(target_arch = "wasm32")]
    {
        wire_vacuum_runtime_hooks(
            Some(Arc::new(|| {
                with_canister_graph_maintenance(|graph| graph.vacuum_stats().queue_len)
            })),
            Some(Arc::new(|| {
                let _ = with_canister_graph_maintenance(|graph| {
                    graph.vacuum_step(MAX_OPS_PER_VACUUM_TICK)
                });
            })),
        );
    }
}

#[cfg(target_arch = "wasm32")]
fn current_vacuum_backlog_len() -> usize {
    VACUUM_BACKLOG_PROVIDER
        .with(|provider| provider.borrow().as_ref().map(|f| f()).unwrap_or_default())
}

#[cfg(target_arch = "wasm32")]
fn desired_vacuum_interval_secs() -> u64 {
    let backlog = current_vacuum_backlog_len();
    if backlog == 0 {
        VACUUM_INTERVAL_IDLE_SECS
    } else if backlog > VACUUM_QUEUE_BUSY_THRESHOLD {
        VACUUM_INTERVAL_BUSY_SECS
    } else {
        PERIODIC_VACUUM_INTERVAL_SECS
    }
}

#[cfg(target_arch = "wasm32")]
fn install_periodic_vacuum_timer(interval_secs: u64) {
    use ic_cdk_timers::set_timer_interval;

    VACUUM_TIMER.with(|slot| {
        if let Some(id) = slot.borrow_mut().take() {
            ic_cdk_timers::clear_timer(id);
        }
        let id = set_timer_interval(
            Duration::from_secs(interval_secs),
            run_periodic_maintenance_tick,
        );
        *slot.borrow_mut() = Some(id);
    });
    VACUUM_TIMER_INTERVAL_SECS.with(|current| *current.borrow_mut() = interval_secs);
}

#[cfg(target_arch = "wasm32")]
fn maybe_reschedule_periodic_vacuum_timer() {
    let desired = desired_vacuum_interval_secs();
    let current = VACUUM_TIMER_INTERVAL_SECS.with(|v| *v.borrow());
    if desired != current {
        install_periodic_vacuum_timer(desired);
    }
}

#[init]
fn canister_init() {
    canister_host::CanisterHost::install_fresh();
    wire_periodic_vacuum_runtime();
    #[cfg(target_arch = "wasm32")]
    install_periodic_vacuum_timer(desired_vacuum_interval_secs());
}

#[pre_upgrade]
fn canister_pre_upgrade() {
    #[cfg(target_arch = "wasm32")]
    VACUUM_TIMER.with(|slot| {
        if let Some(id) = slot.borrow_mut().take() {
            ic_cdk_timers::clear_timer(id);
        }
    });
}

#[post_upgrade]
fn canister_post_upgrade() {
    canister_host::CanisterHost::restore_after_upgrade();
    wire_periodic_vacuum_runtime();
    #[cfg(target_arch = "wasm32")]
    install_periodic_vacuum_timer(desired_vacuum_interval_secs());
}

#[query(name = "query")]
fn query_gql(
    gql: String,
    params: Option<BTreeMap<String, ApiValue>>,
) -> Result<ApiQueryResponse, String> {
    let request = ApiQueryRequest {
        query: gql,
        params: params.unwrap_or_default(),
    };
    let auth = canister_auth_context();
    let run = |query_text: &str| {
        let req = ApiQueryRequest {
            query: query_text.to_owned(),
            params: request.params.clone(),
        };
        with_canister_graph_and_service(|service, graph| {
            match service.execute_api_request_with_service_stable_dirty(graph, &auth, &req, None) {
                Ok((resp, dirty)) => (Ok(resp), dirty),
                Err(e) => (Err(e), false),
            }
        })
    };
    match run(&request.query) {
        Ok(result) => Ok(result),
        Err(GleaphError::Parse(_)) if request.query.contains("\\'") => {
            let normalized = request.query.replace("\\'", "''");
            run(&normalized).map_err(map_err)
        }
        Err(err) => Err(map_err(err)),
    }
}

#[query(name = "explain")]
fn explain_gql(gql: String) -> Result<ApiPlanResponse, String> {
    let request = ApiQueryRequest {
        query: gql,
        params: BTreeMap::new(),
    };
    let auth = canister_auth_context();
    let run = |query_text: &str| {
        let req = ApiQueryRequest {
            query: query_text.to_owned(),
            params: request.params.clone(),
        };
        with_canister_service(|service| service.plan_api_request(&auth, &req, None))
    };
    match run(&request.query) {
        Ok(result) => Ok(result),
        Err(GleaphError::Parse(_)) if request.query.contains("\\'") => {
            let normalized = request.query.replace("\\'", "''");
            run(&normalized).map_err(map_err)
        }
        Err(err) => Err(map_err(err)),
    }
}

#[update(name = "update")]
fn update_gql(
    gql: String,
    params: Option<BTreeMap<String, ApiValue>>,
) -> Result<ApiQueryResponse, String> {
    let request = ApiQueryRequest {
        query: gql,
        params: params.unwrap_or_default(),
    };
    let auth = canister_auth_context();
    let run = |query_text: &str| {
        let req = ApiQueryRequest {
            query: query_text.to_owned(),
            params: request.params.clone(),
        };
        with_canister_graph_and_service(|service, graph| {
            match service
                .execute_update_api_request_with_service_stable_dirty(graph, &auth, &req, None)
            {
                Ok((resp, dirty)) => (Ok(resp), dirty),
                Err(e) => (Err(e), false),
            }
        })
    };
    match run(&request.query) {
        Ok(result) => Ok(result),
        Err(GleaphError::Parse(_)) if request.query.contains("\\'") => {
            let normalized = request.query.replace("\\'", "''");
            run(&normalized).map_err(map_err)
        }
        Err(err) => Err(map_err(err)),
    }
}

fn federation_routed_log(message: impl std::fmt::Display) {
    #[cfg(target_arch = "wasm32")]
    ic_cdk::println!("gleaph-fed {}", message);
    #[cfg(not(target_arch = "wasm32"))]
    drop(message);
}

fn merge_api_execution_batch(parts: Vec<ApiExecutionResult>) -> ApiExecutionResult {
    if parts.is_empty() {
        return ApiExecutionResult {
            rows: Vec::new(),
            warnings: Vec::new(),
            summary: ApiExecutionSummary {
                row_count: 0,
                warning_count: 0,
                had_dml: false,
            },
        };
    }
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    let mut had_dml = false;
    for p in parts {
        rows.extend(p.rows);
        warnings.extend(p.warnings);
        had_dml |= p.summary.had_dml;
    }
    let row_count = rows.len();
    let warning_count = warnings.len();
    ApiExecutionResult {
        rows,
        warnings,
        summary: ApiExecutionSummary {
            row_count,
            warning_count,
            had_dml,
        },
    }
}

fn run_routed_query_execution(
    auth: AuthContext,
    gql: String,
    params: BTreeMap<String, ApiValue>,
) -> Result<ApiExecutionResult, GleaphError> {
    let request = ApiQueryRequest { query: gql, params };
    let run = |query_text: &str| {
        let req = ApiQueryRequest {
            query: query_text.to_owned(),
            params: request.params.clone(),
        };
        with_canister_graph_and_service(|service, graph| {
            match service.execute_api_request_with_service_stable_dirty(graph, &auth, &req, None) {
                Ok((r, dirty)) => (Ok(r.execution), dirty),
                Err(e) => (Err(e), false),
            }
        })
    };
    match run(&request.query) {
        Ok(result) => Ok(result),
        Err(GleaphError::Parse(_)) if request.query.contains("\\'") => {
            let normalized = request.query.replace("\\'", "''");
            run(&normalized)
        }
        Err(err) => Err(err),
    }
}

#[update(name = "execute_subplan_v1")]
async fn execute_subplan_v1(
    wire: SubplanWireV1,
    params: Option<BTreeMap<String, ApiValue>>,
) -> Result<ApiExecutionResult, String> {
    let ops = subplan_wire_v1::wire_v1_to_plan_ops(&wire).map_err(map_err)?;
    let gql = graph_registry::subplan_to_routed_query(&ops).map_err(|e| map_err(e.into()))?;
    let auth = canister_auth_context();
    run_routed_query_execution(auth, gql, params.unwrap_or_default()).map_err(map_err)
}

#[update(name = "execute_routed_query")]
async fn execute_routed_query(
    gql: String,
    params: Option<BTreeMap<String, ApiValue>>,
) -> Result<ApiExecutionResult, String> {
    let auth = canister_auth_context();
    run_routed_query_execution(auth, gql, params.unwrap_or_default()).map_err(map_err)
}

#[update(name = "execute_routed_query_with_subject")]
async fn execute_routed_query_with_subject(
    gql: String,
    params: Option<BTreeMap<String, ApiValue>>,
    query_subject: Option<CandidPrincipal>,
) -> Result<ApiExecutionResult, String> {
    let msg = ic_cdk::api::msg_caller();
    let is_controller = ic_cdk::api::is_controller(&msg);
    let auth = match with_canister_service(|service| {
        service.auth_for_routed_query(msg, is_controller, query_subject)
    }) {
        Ok(a) => a,
        Err(err) => {
            federation_routed_log(format!("delegation_rejected: {err}"));
            return Err(map_err(err));
        }
    };
    federation_routed_log(format!(
        "routed_with_subject rows=1 subject={}",
        query_subject.is_some()
    ));
    run_routed_query_execution(auth, gql, params.unwrap_or_default()).map_err(map_err)
}

#[update(name = "execute_routed_query_batch")]
async fn execute_routed_query_batch(
    gql: String,
    param_rows: Vec<BTreeMap<String, ApiValue>>,
    query_subject: Option<CandidPrincipal>,
) -> Result<ApiExecutionResult, String> {
    if param_rows.is_empty() {
        return Ok(ApiExecutionResult {
            rows: vec![],
            warnings: vec![],
            summary: ApiExecutionSummary {
                row_count: 0,
                warning_count: 0,
                had_dml: false,
            },
        });
    }
    if param_rows.len() > MAX_FEDERATION_ROUTED_PARAM_ROWS {
        return Err(map_err(GleaphError::FederationRoutedQuery(format!(
            "param_rows len {} exceeds max {}",
            param_rows.len(),
            MAX_FEDERATION_ROUTED_PARAM_ROWS
        ))));
    }
    let msg = ic_cdk::api::msg_caller();
    let is_controller = ic_cdk::api::is_controller(&msg);
    let auth = match with_canister_service(|service| {
        service.auth_for_routed_query(msg, is_controller, query_subject)
    }) {
        Ok(a) => a,
        Err(err) => {
            federation_routed_log(format!("delegation_rejected: {err}"));
            return Err(map_err(err));
        }
    };
    federation_routed_log(format!(
        "routed_batch param_rows={} subject={}",
        param_rows.len(),
        query_subject.is_some()
    ));
    let mut parts = Vec::with_capacity(param_rows.len());
    for row in param_rows {
        parts.push(run_routed_query_execution(auth.clone(), gql.clone(), row).map_err(map_err)?);
    }
    Ok(merge_api_execution_batch(parts))
}

#[update]
fn prepare(
    name: String,
    gql: String,
    options: Option<PreparedOptions>,
) -> Result<ApiPrepareResponse, String> {
    let request = ApiPrepareRequest {
        name,
        query: gql,
        options,
    };
    let auth = canister_auth_context();
    with_canister_service_mut_persist(|service| {
        service.prepare_api(&auth, &request, None).map_err(map_err)
    })
}

#[update]
async fn warm_use_graph_cache(gql: String) -> Result<Vec<String>, String> {
    let request = QueryRequest {
        query: gql,
        params: BTreeMap::new(),
    };
    let auth = canister_auth_context();
    let resolver = with_canister_service(|service| service.clone());
    resolver
        .warm_use_graph_cache_for_query(&auth, &request, None)
        .await
        .map_err(map_err)
}

#[update(name = "invalidate_graph_registry_cache")]
fn invalidate_graph_registry_cache(graph_name: Option<String>) -> Result<(), String> {
    let auth = canister_auth_context();
    with_canister_service(|service| {
        service
            .invalidate_graph_registry_cache(&auth, graph_name.as_deref())
            .map_err(map_err)
    })
}

#[query(name = "list_prepared")]
fn list_prepared_canister() -> Result<Vec<PreparedQueryInfo>, String> {
    let auth = canister_auth_context();
    with_canister_service(|service| service.list_prepared(&auth).map_err(map_err))
}

#[query(name = "list_prepared_api")]
fn list_prepared_api_canister() -> Result<ApiListPreparedResponse, String> {
    let auth = canister_auth_context();
    with_canister_service(|service| service.list_prepared_api(&auth).map_err(map_err))
}

#[update]
fn drop_prepared(name: String) -> Result<ApiDropPreparedResponse, String> {
    let request = ApiDropPreparedRequest { name };
    let auth = canister_auth_context();
    with_canister_service_mut_persist(|service| {
        service.drop_prepared_api(&auth, &request).map_err(map_err)
    })
}

#[query(name = "execute_prepared_query")]
fn execute_prepared_query(
    name: String,
    params: BTreeMap<String, ApiValue>,
    sort: Option<Vec<PreparedSortSpec>>,
) -> Result<ApiQueryResponse, String> {
    let request = ApiExecutePreparedRequest { name, params, sort };
    let auth = canister_auth_context();
    with_canister_graph_and_service(|service, graph| {
        match service.execute_prepared_api_request(graph, &auth, &request, None) {
            Ok(resp) => (Ok(resp), false),
            Err(e) => (Err(map_err(e)), false),
        }
    })
}

#[update(name = "execute_prepared_update")]
fn execute_prepared_update(
    name: String,
    params: BTreeMap<String, ApiValue>,
) -> Result<ApiQueryResponse, String> {
    let request = ApiExecutePreparedRequest {
        name,
        params,
        sort: None,
    };
    let auth = canister_auth_context();
    with_canister_graph_and_service(|service, graph| {
        match service.execute_prepared_api_request(graph, &auth, &request, None) {
            Ok(resp) => (Ok(resp), false),
            Err(e) => (Err(map_err(e)), false),
        }
    })
}

#[update]
fn set_acl_entry(principal: CandidPrincipal, level: AccessLevel) -> Result<(), String> {
    let auth = canister_auth_context();
    with_canister_service_mut_persist(|service| {
        service
            .set_acl_entry(&auth, principal.to_text(), level)
            .map_err(map_err)
    })
}

#[update]
fn remove_acl_entry(principal: CandidPrincipal) -> Result<bool, String> {
    let auth = canister_auth_context();
    with_canister_service_mut_persist(|service| {
        service
            .remove_acl_entry(&auth, &principal.to_text())
            .map_err(map_err)
    })
}

#[query]
fn list_acl_entries() -> Result<Vec<AclEntry>, String> {
    let auth = canister_auth_context();
    with_canister_service(|service| service.list_acl_entries(&auth).map_err(map_err))
}

export_candid!();

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prepared::{PreparedOptions, PreparedQueryKind, PreparedSortKey, PreparedSortSpec};
    use gleaph_gql_executor::{
        BindingRow, ExecutionError, ExecutionResultExt, GraphRegistryResolver, GraphResolution,
        OutputRow, ProcedureInvocation, ProcedureRegistry, UseGraphRouter,
    };
    use gleaph_gql_ic::PrincipalValue;
    use gleaph_gql_planner::PlanOp;
    use gleaph_graph_kernel::{GraphRead, GraphWrite, PropertyMap};
    use gleaph_graph_store::GraphStoreVecMemory;
    use gleaph_graph_store::integration::GraphStoreKernelHarness;
    use serde_json::{from_str, to_string};
    use std::collections::BTreeSet;
    use std::sync::Arc;

    fn test_principal(text: &str) -> Principal {
        Principal::from_text(text).expect("valid test principal")
    }

    /// Management / controller id used across tests.
    fn mgmt() -> Principal {
        test_principal("aaaaa-aa")
    }

    /// Distinct user principal for read-level ACL tests.
    fn sample_user() -> Principal {
        test_principal("2vxsx-fae")
    }

    fn new_pma_harness() -> GraphStoreKernelHarness<GraphStoreVecMemory> {
        GraphStoreKernelHarness::bootstrap_empty(GraphStoreVecMemory::default())
            .expect("bootstrap harness")
    }

    fn insert_node_prop<G: GraphWrite>(
        graph: &mut G,
        label: &str,
        key: &str,
        value: Value,
    ) -> gleaph_graph_kernel::NodeRecord {
        let mut props = PropertyMap::new();
        props.insert(key.to_owned(), value);
        graph
            .insert_node(&[label.to_owned()], &props)
            .expect("insert node")
    }

    #[test]
    fn gleaph_error_exposes_graph_error_kind_through_execution() {
        let err = GleaphError::from(ExecutionError::from(GraphError::property_store(
            std::io::Error::other("store"),
        )));
        assert_eq!(err.graph_error_kind(), Some(GraphErrorKind::PropertyStore));
        assert!(matches!(
            err.as_graph_error(),
            Some(GraphError::PropertyStore { .. })
        ));
    }

    #[test]
    fn execute_query_returns_plan_and_execution_dtos() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "name", Value::Text("Alice".to_owned()));

        let query = parse_query("MATCH (n:User) RETURN n.name").expect("parse query");
        let planned = plan_query(&query, None).expect("plan");
        assert!(planned.explain.contains("Plan:"));
        let output =
            execute_query(&mut graph, &query, None, &ExecutionContext::default()).expect("run");

        assert_eq!(output.execution.rows.len(), 1);
        assert_eq!(output.execution.summary.row_count, 1);
        assert!(!output.plan.summary.has_dml);
        assert!(
            output.plan.explain.is_empty(),
            "execute path skips explain text; use plan_query for explain"
        );
    }

    #[test]
    fn canister_graph_runtime_overlay_vacuum_queue_progresses() {
        let before = with_canister_graph(|graph| graph.vacuum_stats());
        let mut inserted_id = None;
        with_canister_graph(|graph| {
            let empty = gleaph_graph_kernel::PropertyMap::new();
            let node = graph
                .insert_node(&["Person".to_owned()], &empty)
                .expect("insert node");
            inserted_id = Some(node.id);
            graph.delete_node(node.id, true).expect("delete node");
        });
        let inserted_id = inserted_id.expect("inserted id");
        let visible = with_canister_graph(|graph| graph.scan_nodes(Some("Person")).expect("scan"));
        assert!(visible.into_iter().all(|n| n.id != inserted_id));

        let processed = with_canister_graph(|graph| graph.vacuum_step(64));
        let after = with_canister_graph(|graph| graph.vacuum_stats());
        assert!(after.free_list_len >= before.free_list_len);
        assert!(after.queue_cursor >= before.queue_cursor.saturating_add(processed as u64));
    }

    #[test]
    fn execute_block_surfaces_dml_summary() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let block =
            parse_block("MATCH (n:User) SET n.name = 'updated' RETURN n").expect("parse block");
        let planned = plan_block(&block, None).expect("plan");
        assert!(planned.explain.contains("Data modification: yes"));
        let output =
            execute_block(&mut graph, &block, None, &ExecutionContext::default()).expect("run");

        assert!(output.plan.summary.has_dml);
        assert!(output.execution.summary.had_dml);
        assert!(output.plan.explain.is_empty());
    }

    #[test]
    fn execute_query_str_parses_and_runs() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "name", Value::Text("Alice".to_owned()));

        let q = "MATCH (n:User) RETURN n.name";
        let query = parse_query(q).expect("parse query");
        let planned = plan_query(&query, None).expect("plan");
        assert!(planned.explain.contains("NodeScan"));
        let output =
            execute_query_str(&mut graph, q, None, &ExecutionContext::default()).expect("run");

        assert_eq!(output.execution.summary.row_count, 1);
        assert!(output.plan.explain.is_empty());
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
    fn plan_request_exposes_use_graph_pushdown_capability() {
        let request = QueryRequest {
            query: "USE myGraph MATCH (n:User)-[:KNOWS]->(m) RETURN m".to_owned(),
            params: BTreeMap::new(),
        };

        let response = plan_request(&request, None).expect("plan request");
        assert_eq!(response.use_graph_pushdown.len(), 1);
        assert_eq!(response.use_graph_pushdown[0].graph_name, "myGraph");
        assert!(response.use_graph_pushdown[0].supported);
    }

    #[test]
    fn plan_api_request_exposes_use_graph_pushdown_capability() {
        let request = ApiQueryRequest {
            query: "USE myGraph MATCH ANY SHORTEST (a)-[:KNOWS]->{1,3}(b) RETURN b".to_owned(),
            params: BTreeMap::new(),
        };

        let response = plan_api_request(&request, None).expect("plan api request");
        assert_eq!(response.use_graph_pushdown.len(), 1);
        assert_eq!(response.use_graph_pushdown[0].graph_name, "myGraph");
        assert!(!response.use_graph_pushdown[0].supported);
        assert!(
            response.use_graph_pushdown[0]
                .reason
                .as_ref()
                .is_some_and(|reason| !reason.is_empty())
        );
        assert_eq!(response.unsupported_use_graph_pushdowns().len(), 1);
    }

    #[test]
    fn execute_request_exposes_use_graph_pushdown_capability() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let request = QueryRequest {
            query: "USE myGraph MATCH (n:User)-[:KNOWS]->(m) RETURN m".to_owned(),
            params: BTreeMap::new(),
        };

        let response = execute_request(&mut graph, &request, None).expect("execute request");
        assert_eq!(response.use_graph_pushdown.len(), 1);
        assert_eq!(response.use_graph_pushdown[0].graph_name, "myGraph");
        assert!(response.use_graph_pushdown[0].supported);
    }

    #[test]
    fn execute_api_request_exposes_use_graph_pushdown_capability() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let request = ApiQueryRequest {
            query: "USE myGraph MATCH ANY SHORTEST (a)-[:KNOWS]->{1,3}(b) RETURN b".to_owned(),
            params: BTreeMap::new(),
        };

        let response = execute_api_request(&mut graph, &request, None).expect("execute api");
        assert_eq!(response.use_graph_pushdown.len(), 1);
        assert_eq!(response.use_graph_pushdown[0].graph_name, "myGraph");
        assert!(!response.use_graph_pushdown[0].supported);
        assert!(
            response
                .execution
                .warnings
                .iter()
                .any(|warning| warning.contains("remote USE GRAPH pushdown unavailable"))
        );
        assert_eq!(response.unsupported_use_graph_pushdowns().len(), 1);
        assert_eq!(response.use_graph_pushdown_warnings().len(), 1);
    }

    #[test]
    fn execute_request_uses_params_and_returns_response_dto() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let request = QueryRequest {
            query: "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid".to_owned(),
            params: [("uid".to_owned(), Value::Text("u1".to_owned()))]
                .into_iter()
                .collect(),
        };

        let response = execute_request(&mut graph, &request, None).expect("execute request");
        assert_eq!(response.execution.summary.row_count, 1);
        assert!(!response.plan_summary.has_dml);
        assert!(response.explain.is_empty());
    }

    #[test]
    fn execute_request_call_db_node_labels() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));
        let _ = insert_node_prop(&mut graph, "Post", "title", Value::Text("hello".to_owned()));

        let request = QueryRequest {
            query: "CALL db.nodeLabels() YIELD label RETURN label".to_owned(),
            params: BTreeMap::new(),
        };
        let response = execute_request(&mut graph, &request, None).expect("execute request");
        let got: BTreeSet<_> = response
            .execution
            .rows
            .iter()
            .filter_map(|row| match row.get("label") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(got, BTreeSet::from(["Post".to_owned(), "User".to_owned()]));
    }

    #[test]
    fn execute_request_call_db_relationship_types_and_property_keys() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let user = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned())).id;
        let post =
            insert_node_prop(&mut graph, "Post", "title", Value::Text("hello".to_owned())).id;
        let mut edge_props = PropertyMap::new();
        edge_props.insert("since".to_owned(), Value::Int64(2024));
        let _ = graph
            .insert_edge(user, post, Some("AUTHORED"), &edge_props, false)
            .expect("insert edge");

        let rel_req = QueryRequest {
            query: "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType"
                .to_owned(),
            params: BTreeMap::new(),
        };
        let rel = execute_request(&mut graph, &rel_req, None).expect("execute rel types");
        assert_eq!(
            rel.execution
                .rows
                .first()
                .and_then(|row| row.get("relationshipType")),
            Some(&Value::Text("AUTHORED".to_owned()))
        );

        let key_req = QueryRequest {
            query: "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey".to_owned(),
            params: BTreeMap::new(),
        };
        let keys = execute_request(&mut graph, &key_req, None).expect("execute property keys");
        let got: BTreeSet<_> = keys
            .execution
            .rows
            .iter()
            .filter_map(|row| match row.get("propertyKey") {
                Some(Value::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            got,
            BTreeSet::from(["since".to_owned(), "title".to_owned(), "uid".to_owned()])
        );
    }

    #[test]
    fn execute_request_call_procedure_rejects_unexpected_args() {
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));
        let request = QueryRequest {
            query: "CALL db.labels(1) YIELD lbl RETURN lbl".to_owned(),
            params: BTreeMap::new(),
        };
        let err = execute_request(&mut graph, &request, None).expect_err("should fail");
        assert!(matches!(
            err,
            GleaphError::Execution(ExecutionError::TypeMismatch(_))
        ));
    }

    #[test]
    fn custom_registry_can_extend_standard_procedures() {
        struct AppRegistry;
        impl ProcedureRegistry for AppRegistry {
            fn call(
                &self,
                _graph: &dyn GraphRead,
                invocation: &ProcedureInvocation,
            ) -> ExecutionResultExt<Vec<OutputRow>> {
                if invocation.name != vec!["app".to_owned(), "echo".to_owned()] {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "CallProcedure.unknown_procedure",
                    ));
                }
                Ok(vec![
                    [(
                        "value".to_owned(),
                        invocation.args.first().cloned().unwrap_or(Value::Null),
                    )]
                    .into_iter()
                    .collect(),
                ])
            }
        }

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let ctx = ExecutionContext {
            procedure_registry: Some(delegated_procedure_registry(Arc::new(AppRegistry))),
            ..ExecutionContext::default()
        };

        let app = execute_query_str(
            &mut graph,
            "CALL app.echo('hi') YIELD value RETURN value",
            None,
            &ctx,
        )
        .expect("app proc");
        assert_eq!(
            app.execution.rows.first().and_then(|row| row.get("value")),
            Some(&Value::Text("hi".to_owned()))
        );

        let builtin = execute_query_str(
            &mut graph,
            "CALL db.nodeLabels() YIELD label RETURN label",
            None,
            &ctx,
        )
        .expect("builtin proc through fallback");
        assert_eq!(
            builtin
                .execution
                .rows
                .first()
                .and_then(|row| row.get("label")),
            Some(&Value::Text("User".to_owned()))
        );
    }

    #[test]
    fn service_set_procedure_registry_applies_to_execute_paths() {
        struct AppOnlyRegistry;
        impl ProcedureRegistry for AppOnlyRegistry {
            fn call(
                &self,
                _graph: &dyn GraphRead,
                invocation: &ProcedureInvocation,
            ) -> ExecutionResultExt<Vec<OutputRow>> {
                if invocation.name != vec!["app".to_owned(), "echo".to_owned()] {
                    return Err(ExecutionError::UnsupportedPlanOp(
                        "CallProcedure.unknown_procedure",
                    ));
                }
                Ok(vec![
                    [("value".to_owned(), Value::Text("ok".to_owned()))]
                        .into_iter()
                        .collect(),
                ])
            }
        }

        let mut service = GleaphService::new();
        service.set_procedure_registry(delegated_procedure_registry(Arc::new(AppOnlyRegistry)));
        let admin = AuthContext::controller(mgmt());
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();

        let request = QueryRequest {
            query: "CALL app.echo() YIELD value RETURN value".to_owned(),
            params: BTreeMap::new(),
        };
        let response = service
            .execute_request(&mut graph, &admin, &request, None)
            .expect("execute app procedure through service");
        assert_eq!(
            response
                .execution
                .rows
                .first()
                .and_then(|row| row.get("value")),
            Some(&Value::Text("ok".to_owned()))
        );

        // Built-ins are still reachable through delegated fallback.
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));
        let built_in = QueryRequest {
            query: "CALL db.nodeLabels() YIELD label RETURN label".to_owned(),
            params: BTreeMap::new(),
        };
        let built_in_response = service
            .execute_request(&mut graph, &admin, &built_in, None)
            .expect("execute built-in through delegated fallback");
        assert_eq!(
            built_in_response
                .execution
                .rows
                .first()
                .and_then(|row| row.get("label")),
            Some(&Value::Text("User".to_owned()))
        );
    }

    #[test]
    fn service_set_graph_registry_resolver_enables_use_graph_resolution() {
        struct StaticResolver;
        impl GraphRegistryResolver for StaticResolver {
            fn resolve(
                &self,
                requested_graph: &str,
                _caller: Option<&Value>,
            ) -> ExecutionResultExt<GraphResolution> {
                if requested_graph == "tenant.main" {
                    return Ok(GraphResolution {
                        graph_name: "tenant.main".to_owned(),
                        canister_id: None,
                    });
                }
                Err(ExecutionError::InvalidPlan("graph not found".to_owned()))
            }
        }

        let mut service = GleaphService::new();
        service.set_graph_registry_resolver(Arc::new(StaticResolver));
        let admin = AuthContext::controller(mgmt());
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let request = QueryRequest {
            query: "USE tenant.main CALL db.nodeLabels() YIELD label RETURN label".to_owned(),
            params: BTreeMap::new(),
        };
        let response = service
            .execute_request(&mut graph, &admin, &request, None)
            .expect("use graph with resolver");
        assert_eq!(
            response
                .execution
                .rows
                .first()
                .and_then(|row| row.get("label")),
            Some(&Value::Text("User".to_owned()))
        );
    }

    #[test]
    fn service_set_use_graph_router_delegates_remote_use_graph() {
        struct RemoteResolver;
        impl GraphRegistryResolver for RemoteResolver {
            fn resolve(
                &self,
                requested_graph: &str,
                _caller: Option<&Value>,
            ) -> ExecutionResultExt<GraphResolution> {
                Ok(GraphResolution {
                    graph_name: requested_graph.to_owned(),
                    canister_id: Some("rrkah-fqaaa-aaaaa-aaaaq-cai".to_owned()),
                })
            }
        }

        struct MockRouter;
        #[async_trait::async_trait(?Send)]
        impl UseGraphRouter for MockRouter {
            async fn execute_remote_subplan(
                &self,
                _target: &GraphResolution,
                _sub_plan: &[PlanOp],
                _ctx: &ExecutionContext,
                _input_rows: Vec<BindingRow>,
            ) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
                Ok((
                    vec![std::collections::BTreeMap::from([(
                        std::rc::Rc::<str>::from("label"),
                        gleaph_gql_executor::BindingValue::Scalar(Value::Text(
                            "RemoteUser".to_owned(),
                        )),
                    )])],
                    None,
                ))
            }
        }

        let mut service = GleaphService::new();
        service.set_graph_registry_resolver(Arc::new(RemoteResolver));
        service.set_use_graph_router(Arc::new(MockRouter));
        let admin = AuthContext::controller(mgmt());
        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();

        let request = QueryRequest {
            query: "USE remote.graph CALL db.nodeLabels() YIELD label RETURN label".to_owned(),
            params: BTreeMap::new(),
        };
        let response = service
            .execute_request(&mut graph, &admin, &request, None)
            .expect("remote delegated through router");
        assert_eq!(
            response
                .execution
                .rows
                .first()
                .and_then(|row| row.get("label")),
            Some(&Value::Text("RemoteUser".to_owned()))
        );
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

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let response =
            execute_api_request(&mut graph, &request, None).expect("execute api request");
        let json = to_string(&response).expect("serialize response");
        let decoded: ApiQueryResponse = from_str(&json).expect("deserialize response");
        assert_eq!(decoded.execution.summary.row_count, 1);
        assert!(!decoded.plan_summary.has_dml);
    }

    #[test]
    fn api_value_principal_round_trips_through_json_and_value() {
        let principal = mgmt();
        let api = ApiValue::Principal(principal);
        let json = to_string(&api).expect("serialize principal api value");
        let decoded: ApiValue = from_str(&json).expect("deserialize principal api value");
        assert_eq!(decoded, api);

        let gql_value = Value::from(&api);
        let Value::Extension(ext) = &gql_value else {
            panic!("expected principal extension value");
        };
        assert_eq!(ext.type_name(), "ic.Principal");

        let back = ApiValue::from(&gql_value);
        assert_eq!(back, api);
    }

    #[test]
    fn anonymous_can_execute_prepared_query_but_not_query_or_prepare() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let anonymous = AuthContext::anonymous();
        service
            .prepare(
                &admin,
                "users_by_uid",
                "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid",
                None,
                None,
            )
            .expect("prepare");

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let params = [("uid".to_owned(), Value::Text("u1".to_owned()))]
            .into_iter()
            .collect();
        let response = service
            .execute_prepared(&mut graph, &anonymous, "users_by_uid", &params, None, None)
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
            .prepare(&anonymous, "denied", "MATCH (n) RETURN n", None, None)
            .expect_err("anonymous prepare should be denied");
        assert!(matches!(err, GleaphError::PermissionDenied { .. }));
    }

    #[test]
    fn anonymous_can_execute_prepared_update_only() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let anonymous = AuthContext::anonymous();
        let prepared = service
            .prepare(
                &admin,
                "set_user_name",
                "MATCH (n:User) SET n.name = 'updated' RETURN n",
                None,
                None,
            )
            .expect("prepare");
        assert_eq!(prepared.kind, PreparedQueryKind::Update);

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "name", Value::Text("before".to_owned()));

        let response = service
            .execute_prepared(
                &mut graph,
                &anonymous,
                "set_user_name",
                &BTreeMap::new(),
                None,
                None,
            )
            .expect("anonymous prepared update");
        assert!(response.execution.summary.had_dml);
    }

    #[test]
    fn read_user_can_query_and_list_prepared_but_not_prepare() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let reader_p = sample_user();
        let reader = AuthContext::principal(reader_p);
        service
            .set_acl_entry(&admin, reader_p.to_text(), AccessLevel::Read)
            .expect("set acl");
        service
            .prepare(&admin, "users", "MATCH (n:User) RETURN n", None, None)
            .expect("prepare");

        let listed = service
            .list_prepared(&reader)
            .expect("reader list prepared");
        assert_eq!(listed.len(), 1);

        let err = service
            .prepare(&reader, "denied", "MATCH (n) RETURN n", None, None)
            .expect_err("reader prepare should be denied");
        assert!(matches!(err, GleaphError::PermissionDenied { .. }));
    }

    #[test]
    fn prepared_info_exposes_parameter_metadata_and_type_warnings() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let prepared = service
            .prepare(
                &admin,
                "warn_query",
                "MATCH (n:User) WHERE n.uid = $uid RETURN abs('oops')",
                None,
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
    fn prepared_query_info_detects_caller_usage() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let prepared = service
            .prepare(
                &admin,
                "caller_query",
                "MATCH (n:User) WHERE n.uid = caller() RETURN n.uid AS uid",
                None,
                None,
            )
            .expect("prepare");
        assert!(prepared.requires_caller);

        let plain = service
            .prepare(
                &admin,
                "plain_query",
                "MATCH (n:User) RETURN n.uid AS uid",
                None,
                None,
            )
            .expect("prepare plain");
        assert!(!plain.requires_caller);
    }

    #[test]
    fn caller_function_uses_auth_context_caller_in_prepared_execute() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let reader_p = sample_user();
        let reader = AuthContext::principal(reader_p);
        service
            .set_acl_entry(&admin, reader_p.to_text(), AccessLevel::Read)
            .expect("set acl");
        service
            .prepare(
                &admin,
                "my_docs",
                "MATCH (n:User) WHERE n.uid = caller() RETURN n.uid AS uid",
                None,
                None,
            )
            .expect("prepare");

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(
            &mut graph,
            "User",
            "uid",
            Value::Extension(Box::new(PrincipalValue(reader_p))),
        );
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("other".to_owned()));

        let response = service
            .execute_prepared(&mut graph, &reader, "my_docs", &BTreeMap::new(), None, None)
            .expect("execute prepared");
        assert_eq!(response.execution.summary.row_count, 1);
        let expected_uid: Value = PrincipalValue(reader_p).into();
        assert_eq!(
            response
                .execution
                .rows
                .first()
                .and_then(|row| row.get("uid"))
                .cloned(),
            Some(expected_uid)
        );
    }

    #[test]
    fn caller_function_can_return_principal_extension() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        service
            .prepare(&admin, "whoami", "RETURN caller() AS me", None, None)
            .expect("prepare");

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let response = service
            .execute_prepared(&mut graph, &admin, "whoami", &BTreeMap::new(), None, None)
            .expect("execute prepared");
        let value = response
            .execution
            .rows
            .first()
            .and_then(|row| row.get("me"))
            .expect("me column");
        let Value::Extension(ext) = value else {
            panic!("expected extension value");
        };
        assert_eq!(ext.type_name(), "ic.Principal");
    }

    #[test]
    fn prepare_rejects_unregistered_extension_type() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let err = service
            .prepare(
                &admin,
                "ext_q",
                "MATCH (n:User) WHERE n.identity IS TYPED CUSTOM_FOO RETURN n",
                None,
                None,
            )
            .expect_err("unsupported extension type should error");
        assert!(matches!(err, GleaphError::UnsupportedExtensionType(_)));
    }

    #[test]
    fn prepare_accepts_registered_extension_type() {
        let mut service = GleaphService::new();
        service.register_extension_type("PRINCIPAL");
        let admin = AuthContext::controller(mgmt());
        let prepared = service
            .prepare(
                &admin,
                "ext_q_ok",
                "MATCH (n:User) WHERE n.identity IS TYPED PRINCIPAL RETURN n",
                None,
                None,
            )
            .expect("registered extension type should pass");
        assert!(
            prepared
                .extension_types
                .iter()
                .any(|name| name == "PRINCIPAL")
        );
    }

    #[test]
    fn plan_and_execute_validate_query_expression_extension_types() {
        let service = GleaphService::new();
        let auth = AuthContext::controller(mgmt());
        let request = QueryRequest {
            query:
                "MATCH (n:User) WHERE CAST(n.identity AS CUSTOM_FOO) IS TYPED CUSTOM_FOO RETURN n"
                    .to_owned(),
            params: BTreeMap::new(),
        };

        let plan_err = service
            .plan_request(&auth, &request, None)
            .expect_err("unsupported extension type should fail plan validation");
        assert!(matches!(plan_err, GleaphError::UnsupportedExtensionType(_)));

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let exec_err = service
            .execute_request(&mut graph, &auth, &request, None)
            .expect_err("unsupported extension type should fail execute validation");
        assert!(matches!(exec_err, GleaphError::UnsupportedExtensionType(_)));
    }

    #[test]
    fn prepare_api_exposes_use_graph_pushdown_capability() {
        let mut service = GleaphService::new();
        let auth = AuthContext::controller(mgmt());
        let request = ApiPrepareRequest {
            name: "focused_q".to_owned(),
            query: "USE myGraph MATCH (n:User)-[:KNOWS]->(m) RETURN m".to_owned(),
            options: None,
        };

        let response = service
            .prepare_api(&auth, &request, None)
            .expect("prepare api");
        assert_eq!(response.prepared.use_graph_pushdown.len(), 1);
        assert_eq!(
            response.prepared.use_graph_pushdown[0].graph_name,
            "myGraph"
        );
        assert!(response.prepared.use_graph_pushdown[0].supported);
        assert!(
            response
                .prepared
                .unsupported_use_graph_pushdowns()
                .is_empty()
        );
    }

    #[test]
    fn query_response_filters_only_use_graph_pushdown_warnings() {
        let response = ApiQueryResponse {
            explain: "Plan:".to_owned(),
            plan_summary: ApiPlanSummary {
                estimated_rows: None,
                estimated_cost: None,
                has_dml: false,
                dml_error_count: 0,
                dml_warning_count: 0,
                type_warning_count: 0,
            },
            use_graph_pushdown: vec![],
            execution: ApiExecutionResult {
                rows: vec![],
                warnings: vec![
                    "remote USE GRAPH pushdown unavailable for myGraph: unsupported".to_owned(),
                    "[TYPE] at 0..0: sample".to_owned(),
                ],
                summary: ApiExecutionSummary {
                    row_count: 0,
                    warning_count: 2,
                    had_dml: false,
                },
            },
        };

        assert_eq!(
            response.use_graph_pushdown_warnings(),
            vec!["remote USE GRAPH pushdown unavailable for myGraph: unsupported".to_owned()]
        );
    }

    #[test]
    fn prepared_api_dtos_are_serializable_and_executable() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let anonymous = AuthContext::anonymous();

        let prepare_request = ApiPrepareRequest {
            name: "users_by_uid".to_owned(),
            query: "MATCH (n:User) WHERE n.uid = $uid RETURN n.uid AS uid".to_owned(),
            options: None,
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

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let execute_request = ApiExecutePreparedRequest {
            name: "users_by_uid".to_owned(),
            params: [("uid".to_owned(), ApiValue::Text("u1".to_owned()))]
                .into_iter()
                .collect(),
            sort: None,
        };
        let response = service
            .execute_prepared_api_request(&mut graph, &anonymous, &execute_request, None)
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
            caller: Some(mgmt().to_text()),
            is_controller: true,
        };
        let anonymous_endpoint = ApiAuthContext::default();

        let prepare_endpoint = ApiPrepareEndpointRequest {
            auth: admin_endpoint.clone(),
            request: ApiPrepareRequest {
                name: "public_users".to_owned(),
                query: "MATCH (n:User) RETURN n.uid AS uid".to_owned(),
                options: None,
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

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Text("u1".to_owned()));

        let execute_endpoint = ApiExecutePreparedEndpointRequest {
            auth: anonymous_endpoint,
            request: ApiExecutePreparedRequest {
                name: "public_users".to_owned(),
                params: BTreeMap::new(),
                sort: None,
            },
        };
        let response = service
            .execute_prepared_api_endpoint(&mut graph, &execute_endpoint, None)
            .expect("execute endpoint");
        assert_eq!(response.execution.summary.row_count, 1);
    }

    #[test]
    fn prepared_dynamic_sort_applies_at_execute() {
        let mut service = GleaphService::new();
        let admin = AuthContext::controller(mgmt());
        let opts = PreparedOptions {
            description: None,
            allowed_sorts: vec![PreparedSortKey {
                key: "uid".into(),
                expr: "n.uid".into(),
            }],
            default_sort: None,
        };
        service
            .prepare(
                &admin,
                "users_by_uid",
                "MATCH (n:User) RETURN n.uid AS uid",
                Some(&opts),
                None,
            )
            .expect("prepare");

        let mut harness = new_pma_harness();
        let mut graph = harness.bind_overlay();
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Int64(2));
        let _ = insert_node_prop(&mut graph, "User", "uid", Value::Int64(1));

        let sort_asc = vec![PreparedSortSpec {
            key: "uid".into(),
            descending: false,
            nulls_first: None,
        }];
        let r = service
            .execute_prepared(
                &mut graph,
                &admin,
                "users_by_uid",
                &BTreeMap::new(),
                Some(&sort_asc),
                None,
            )
            .expect("exec");
        assert_eq!(
            r.execution.rows.first().and_then(|row| row.get("uid")),
            Some(&Value::Int64(1))
        );

        let sort_desc = vec![PreparedSortSpec {
            key: "uid".into(),
            descending: true,
            nulls_first: None,
        }];
        let r2 = service
            .execute_prepared(
                &mut graph,
                &admin,
                "users_by_uid",
                &BTreeMap::new(),
                Some(&sort_desc),
                None,
            )
            .expect("exec2");
        assert_eq!(
            r2.execution.rows.first().and_then(|row| row.get("uid")),
            Some(&Value::Int64(2))
        );
    }

    #[test]
    fn federation_auth_for_routed_query_rejects_untrusted_subject() {
        let service = GleaphService::new();
        let peer = sample_user();
        let subject = mgmt();
        let err = service
            .auth_for_routed_query(peer, false, Some(subject))
            .expect_err("untrusted");
        assert!(matches!(err, GleaphError::FederationRoutedQuery(_)));
    }

    #[test]
    fn federation_auth_for_routed_query_accepts_trusted_peer_subject() {
        let mut service = GleaphService::new();
        let peer = sample_user();
        let subject = mgmt();
        service.add_federation_trusted_caller(peer);
        let auth = service
            .auth_for_routed_query(peer, false, Some(subject))
            .expect("ok");
        assert_eq!(auth.query_subject, Some(subject));
    }
}
