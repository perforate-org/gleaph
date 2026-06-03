//! Per-query execution context threaded through plan operators.

use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql_planner::plan::AggregateSpec;
use gleaph_graph_kernel::entry::PreparedWeightDecoder;
use gleaph_graph_kernel::plan_exec::ResolvedLabelTable;

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;

/// Fixed inputs for one [`super::ops::execute_ops_from`] run.
#[derive(Clone)]
pub(crate) struct ExecuteCtx<'a> {
    pub store: &'a GraphStore,
    pub parameters: &'a BTreeMap<String, Value>,
    pub index: Option<&'a dyn PropertyIndexLookup>,
    pub execution: GqlExecutionContext,
    pub gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
}

impl<'a> ExecuteCtx<'a> {
    #[inline]
    pub fn new(
        store: &'a GraphStore,
        parameters: &'a BTreeMap<String, Value>,
        index: Option<&'a dyn PropertyIndexLookup>,
        execution: GqlExecutionContext,
        gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
    ) -> Self {
        Self {
            store,
            parameters,
            index,
            execution,
            gleaph_weight_decoders,
        }
    }

    #[inline]
    pub fn caller(&self) -> Option<Principal> {
        self.execution.caller
    }

    #[inline]
    pub fn expr_evaluator<'b>(
        &'b self,
        aggregate_specs: Option<&'b [AggregateSpec]>,
    ) -> QueryExprEvaluator<'b> {
        QueryExprEvaluator {
            store: self.store,
            parameters: self.parameters,
            aggregate_specs,
            caller: self.caller(),
            resolved_labels: self.execution.resolved_labels.as_ref(),
            gleaph_weight_decoders: self.gleaph_weight_decoders,
        }
    }
}

/// Expression evaluator with access to the active query context.
pub(crate) struct QueryExprEvaluator<'a> {
    pub store: &'a GraphStore,
    pub parameters: &'a BTreeMap<String, Value>,
    /// When set, `ExprKind::Aggregate` reads precomputed results from the row.
    pub aggregate_specs: Option<&'a [AggregateSpec]>,
    /// IC caller for runtime functions such as `MSG_CALLER()`.
    pub caller: Option<Principal>,
    /// Router-resolved labels available to this execution.
    pub resolved_labels: Option<&'a ResolvedLabelTable>,
    /// Prepared decoders for `GLEAPH.WEIGHT(edgeVar)` (when the query uses it).
    pub gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
}
