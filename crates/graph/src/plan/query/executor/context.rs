//! Per-query execution context threaded through plan operators.

use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql_planner::plan::AggregateSpec;
use gleaph_graph_kernel::entry::PreparedWeightDecoder;
use gleaph_graph_kernel::federation::ElementIdEncodingKey;
use gleaph_graph_kernel::plan_exec::{ResolvedLabelTable, ResolvedPropertyTable};

use crate::facade::GraphStore;
use crate::federation::StandaloneFederation;
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
    pub federation: StandaloneFederation,
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
            federation: StandaloneFederation::from_store(store),
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
            resolved_properties: self.execution.resolved_properties.as_ref(),
            gleaph_weight_decoders: self.gleaph_weight_decoders,
            element_id_key: crate::element_id_encoding::resolve_or_host_fixture(
                self.execution.element_id_encoding_key(),
            ),
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
    /// Router-resolved properties available to this execution.
    pub resolved_properties: Option<&'a ResolvedPropertyTable>,
    /// Prepared decoders for `GLEAPH.WEIGHT(edgeVar)` (when the query uses it).
    pub gleaph_weight_decoders: Option<&'a BTreeMap<String, PreparedWeightDecoder>>,
    /// Router-issued per-graph element-id encoding key (ADR 0019), owned by this evaluator so
    /// `ELEMENT_ID`/path encoding never reads ambient thread-local state across an `await`.
    pub element_id_key: ElementIdEncodingKey,
}

impl<'a> QueryExprEvaluator<'a> {
    pub(crate) fn resolved_property_id(
        &self,
        name: &str,
    ) -> Option<gleaph_graph_kernel::entry::PropertyId> {
        if let Some(properties) = self.resolved_properties {
            return properties
                .properties
                .iter()
                .find(|property| property.name == name)
                .map(|property| property.id);
        }
        #[cfg(any(test, feature = "canbench"))]
        {
            Some(crate::test_labels::property_id_for_name(name))
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            None
        }
    }
}
