//! Shared fixtures for [`super`] executor tests.

use std::any::Any;
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::fmt;

use async_trait::async_trait;
use candid::Principal;
use gleaph_gql::parser;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql::types::PathElement;
use gleaph_gql::value::{ExtensionSortableKey, ExtensionValue};
use gleaph_gql_planner::plan::PhysicalPlan;
use gleaph_gql_planner::{PlanBuildOptions, build_plan_with_schema_and_options};

use super::context::QueryExprEvaluator;
use super::super::{GLEAPH_PATH_EXTENSION_HANDLER, materialize};
use crate::facade::FederationRouting;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;

pub use std::collections::{BTreeMap, BTreeSet};
pub use std::rc::Rc;
pub use super::PlanQueryExecutor;
pub(crate) use super::bindings::EdgeBinding;
pub(crate) use super::context::ExecuteCtx;
pub(crate) use super::expand::{execute_expand, ExpandDst};
pub(crate) use super::join::merge_rows;
pub(crate) use super::{
    edge_binding_for_expand, EdgeSequenceOrder, execute_plan_query, execute_plan_query_bindings,
};
pub use gleaph_gql::{Value, value_to_index_key_bytes};
pub use gleaph_gql::token::Span;
pub use gleaph_gql::ast::{
    AggregateFunc, BinaryOp, CmpOp, Expr, ExprKind, NullOrder, ObjectName, OrderByClause, SetOp,
    SortDirection, SortItem, Statement, WhenClause,
};
pub use gleaph_gql::types::{EdgeDirection, LabelExpr};
pub use gleaph_gql_planner::plan::{
    AggregateSpec, ConditionalScanCandidate, PlanOp, ProjectColumn, ScanValue, ShortestMode,
    ShortestPathCost, Str, VarLenSpec, WcojEdge,
};
pub use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeSlotIndex};
pub use gleaph_graph_kernel::federation::FederatedExpandNeighbor;
pub use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};
pub use gleaph_graph_kernel::path::{GraphPathEdgeId, GraphPathVertexId};
pub use ic_stable_lara::VertexId;
pub use crate::facade::EdgeHandle;
pub use crate::facade::GraphStore;
pub use crate::facade::mutation_executor::GraphMutationExecutor;
pub use crate::gql_execution_context::GqlExecutionContext;
pub use super::super::row::PlanRow;
pub use super::PlanBinding;
pub use super::PlanQueryResult;
pub use super::super::error::PlanQueryError;
pub use crate::index::placement::native_test_register_physical_placement;

#[derive(Default)]
pub struct MockPropertyIndex {
    pub equal_hits: RefCell<Vec<PostingHit>>,
    pub range_hits: RefCell<Vec<PostingHit>>,
    pub equal_calls: RefCell<Vec<(u32, Vec<u8>)>>,
    pub range_calls: RefCell<Vec<(u32, PostingRangeRequest)>>,
}

#[async_trait(?Send)]
impl PropertyIndexLookup for MockPropertyIndex {
    async fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        self.equal_calls.borrow_mut().push((property_id, value));
        Ok(self.equal_hits.borrow().clone())
    }

    async fn lookup_range(
        &self,
        property_id: u32,
        req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        self.range_calls
            .borrow_mut()
            .push((property_id, req.clone()));
        Ok(self.range_hits.borrow().clone())
    }

    fn local_shard_id(&self) -> u32 {
        0
    }

    async fn posting_insert_at(
        &self,
        _shard_id: u32,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn posting_remove_at(
        &self,
        _shard_id: u32,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }
}

pub fn plan(ops: Vec<PlanOp>) -> PhysicalPlan {
    PhysicalPlan::from_ops(ops)
}

pub fn plan_gql(input: &str) -> PhysicalPlan {
    let program = parser::parse(input).unwrap_or_else(|err| panic!("parse error: {err}"));
    let tx = program
        .transaction_activity
        .expect("expected transaction activity");
    let block = tx.body.expect("expected statement block");
    let Statement::Query(composite) = &block.first else {
        panic!("expected query statement");
    };
    build_plan_with_schema_and_options(
        &composite.left,
        PlanBuildOptions {
            stats: None,
            path_extensions: &GLEAPH_PATH_EXTENSION_HANDLER,
        },
        &NoSchema,
    )
    .expect("plan should build")
}

pub fn prop(variable: &str, property: &str) -> Expr {
    Expr::new(ExprKind::PropertyAccess {
        expr: Box::new(Expr::new(ExprKind::Variable(variable.to_owned()))),
        property: property.to_owned(),
    })
}

pub fn var(variable: &str) -> Expr {
    Expr::new(ExprKind::Variable(variable.to_owned()))
}

pub fn order_by(items: Vec<SortItem>) -> OrderByClause {
    OrderByClause {
        span: Span::DUMMY,
        items,
    }
}

pub fn sort_item(
    expr: Expr,
    direction: Option<SortDirection>,
    null_order: Option<NullOrder>,
) -> SortItem {
    SortItem {
        span: Span::DUMMY,
        expr,
        direction,
        null_order,
    }
}

pub fn project(expr: Expr, alias: &str) -> ProjectColumn {
    ProjectColumn {
        expr,
        alias: Some(alias.into()),
    }
}

pub fn params() -> BTreeMap<String, Value> {
    BTreeMap::new()
}

pub fn reset_node_scan_visits() {
    super::scan::NODE_SCAN_VISITS.with(|visits| visits.set(0));
}

pub fn node_scan_visits() -> usize {
    super::scan::NODE_SCAN_VISITS.with(|visits| visits.get())
}

#[derive(Clone, Debug)]
pub struct TestOrderableExt(u8);

impl fmt::Display for TestOrderableExt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TestOrderableExt({})", self.0)
    }
}

impl ExtensionValue for TestOrderableExt {
    fn type_name(&self) -> &str {
        "TestOrderableExt"
    }

    fn clone_box(&self) -> Box<dyn ExtensionValue> {
        Box::new(self.clone())
    }

    fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|o| self.0 == o.0)
    }

    fn cmp_ext(&self, other: &dyn ExtensionValue) -> Option<Ordering> {
        other
            .as_any()
            .downcast_ref::<Self>()
            .map(|o| self.0.cmp(&o.0))
    }

    fn sortable_index_key(&self) -> Option<ExtensionSortableKey<'_>> {
        Some(ExtensionSortableKey {
            domain: Cow::Borrowed("test.orderable/v1"),
            bytes: Cow::Owned(vec![self.0]),
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Owned(vec![self.0]))
    }
}

#[derive(Clone, Debug)]
pub struct TestNonOrderableExt;

impl fmt::Display for TestNonOrderableExt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TestNonOrderableExt")
    }
}

impl ExtensionValue for TestNonOrderableExt {
    fn type_name(&self) -> &str {
        "TestNonOrderableExt"
    }

    fn clone_box(&self) -> Box<dyn ExtensionValue> {
        Box::new(self.clone())
    }

    fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
        other.as_any().downcast_ref::<Self>().is_some()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Borrowed(&[0]))
    }
}

pub fn orderable_ext(value: u8) -> Value {
    Value::Extension(Box::new(TestOrderableExt(value)))
}

pub fn non_orderable_ext() -> Value {
    Value::Extension(Box::new(TestNonOrderableExt))
}

/// Minimal [`AggregateSpec`] for tests (no `expr2` / `filter` / `order_by`).
pub fn agg_spec(
    func: AggregateFunc,
    expr: Option<Expr>,
    distinct: bool,
    alias: Option<&str>,
) -> AggregateSpec {
    AggregateSpec {
        func,
        expr,
        expr2: None,
        distinct,
        filter: None,
        order_by: None,
        alias: alias.map(|a| a.into()),
    }
}

pub fn text_column(result: &PlanQueryResult, column: &str) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| match row.get(column) {
            Some(Value::Text(value)) => value.clone(),
            other => panic!("expected text column {column}, got {other:?}"),
        })
        .collect()
}

pub fn bytes_column<'a>(result: &'a PlanQueryResult, column: &str) -> &'a [u8] {
    match result.rows.first().and_then(|row| row.get(column)) {
        Some(Value::Bytes(value)) => value,
        other => panic!("expected bytes column {column}, got {other:?}"),
    }
}

pub fn configure_test_index(store: &GraphStore) {
    store
        .set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            shard_id: 7,
        }))
        .expect("set index routing");
}

pub fn configure_test_federation(store: &GraphStore) {
    configure_test_index(store);
    store
        .set_logical_graph_name(Some("tenant.main".into()))
        .expect("graph name");
    placement::native_test_register_shard(gleaph_graph_kernel::federation::ShardRegistryEntry {
        shard_id: 7,
        graph_canister: Principal::management_canister(),
        index_canister: Principal::management_canister(),
        logical_graph_name: "tenant.main".into(),
        registered_at_ns: 0,
    });
}

pub fn agg_count_star() -> Expr {
    Expr::new(ExprKind::Aggregate {
        func: AggregateFunc::CountStar,
        expr: None,
        expr2: None,
        distinct: false,
        order_by: None,
        filter: None,
    })
}

pub fn eval_test_expr(expr: Expr) -> Value {
    let store = GraphStore::new();
    let params = params();
    let evaluator = QueryExprEvaluator {
        store: &store,
        parameters: &params,
        aggregate_specs: None,
        caller: None,
        gleaph_weight_decoders: None,
    };
    evaluator
        .eval_expr(&PlanRow::new(), &expr)
        .expect("eval test expr")
}

