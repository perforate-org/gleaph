//! Per-invocation context for GQL execution (canister caller, runtime functions).

use std::fmt;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::ObjectName;
use gleaph_gql_ic::principal_to_value;
use gleaph_graph_kernel::entry::{EdgeLabelId, EdgePayloadProfile, PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::ElementIdEncodingKey;
use gleaph_graph_kernel::plan_exec::{
    ResolvedLabelTable, ResolvedPropertyTable, UniqueClaimDispatch,
};

/// Carries data that is fixed for one GQL execution (adhoc, prepared, or plan replay).
#[derive(Clone, Debug, Default)]
pub struct GqlExecutionContext {
    /// Internet Computer caller principal when executing on a canister.
    pub caller: Option<Principal>,
    /// Router-resolved label names for this execution.
    pub resolved_labels: Option<ResolvedLabelTable>,
    /// Router-resolved property names for this execution.
    pub resolved_properties: Option<ResolvedPropertyTable>,
    /// Router-issued per-graph key for ELEMENT_ID and path element encoding.
    pub element_id_encoding_key: Option<[u8; 16]>,
    /// Cross-shard uniqueness claims the canonical segment must `Acquire` for the element it creates
    /// (ADR 0030 slice 5). Empty for non-constrained operations.
    pub unique_claims: Vec<UniqueClaimDispatch>,
}

impl GqlExecutionContext {
    pub fn resolved_vertex_label_id(&self, name: &str) -> Option<VertexLabelId> {
        if let Some(labels) = &self.resolved_labels {
            return labels
                .vertex
                .iter()
                .find(|label| label.name == name)
                .map(|label| label.id);
        }
        #[cfg(any(test, feature = "canbench"))]
        {
            Some(crate::test_labels::vertex_label_id_for_name(name))
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            None
        }
    }

    pub fn resolved_edge_label_id(&self, name: &str) -> Option<EdgeLabelId> {
        if let Some(labels) = &self.resolved_labels {
            return labels
                .edge
                .iter()
                .find(|label| label.name == name)
                .map(|label| label.id);
        }
        #[cfg(any(test, feature = "canbench"))]
        {
            Some(crate::test_labels::edge_label_id_for_name(name))
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            None
        }
    }

    pub fn resolved_edge_label_name(&self, id: EdgeLabelId) -> Option<String> {
        if let Some(labels) = &self.resolved_labels {
            return labels
                .edge
                .iter()
                .find(|label| label.id == id)
                .map(|label| label.name.clone());
        }
        #[cfg(any(test, feature = "canbench"))]
        {
            crate::test_labels::edge_label_name_for_id(id)
        }
        #[cfg(not(any(test, feature = "canbench")))]
        {
            None
        }
    }

    pub fn requires_resolved_labels(&self) -> bool {
        self.resolved_labels.is_some()
    }

    pub fn resolved_edge_payload_profile(&self, id: EdgeLabelId) -> EdgePayloadProfile {
        crate::edge_payload_schema::lookup_edge_payload_profile_with(
            self.resolved_labels.as_ref(),
            id,
        )
    }

    pub fn resolved_property_id(&self, name: &str) -> Option<PropertyId> {
        if let Some(properties) = &self.resolved_properties {
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

    pub fn requires_resolved_properties(&self) -> bool {
        self.resolved_properties.is_some()
    }

    pub fn element_id_encoding_key(&self) -> Option<ElementIdEncodingKey> {
        self.element_id_encoding_key.map(ElementIdEncodingKey)
    }
}

#[cfg(any(test, feature = "canbench"))]
impl GqlExecutionContext {
    /// Host graph tests without router registration (ADR 0019 `host_test_fixture` key).
    pub fn with_host_test_element_id_key() -> Self {
        Self {
            element_id_encoding_key: Some(ElementIdEncodingKey::host_test_fixture().0),
            ..Self::default()
        }
    }
}

/// Errors from supported runtime extension functions (e.g. `MSG_CALLER()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeFunctionError {
    /// `MSG_CALLER()` was evaluated without a canister caller (e.g. host tests).
    MissingCallerContext {
        function: &'static str,
    },
    InvalidArity {
        function: &'static str,
        expected: usize,
        got: usize,
    },
    DistinctNotSupported {
        function: &'static str,
    },
    QualifiedNameNotSupported {
        name: String,
    },
}

impl fmt::Display for RuntimeFunctionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCallerContext { function } => write!(
                f,
                "runtime function {function} requires a canister caller context"
            ),
            Self::InvalidArity {
                function,
                expected,
                got,
            } => write!(
                f,
                "runtime function {function} expects {expected} argument(s), got {got}"
            ),
            Self::DistinctNotSupported { function } => {
                write!(f, "runtime function {function} does not support DISTINCT")
            }
            Self::QualifiedNameNotSupported { name } => {
                write!(f, "runtime function name must be unqualified; got {name:?}")
            }
        }
    }
}

impl std::error::Error for RuntimeFunctionError {}

/// Evaluate a known runtime [`gleaph_gql::ast::ExprKind::FunctionCall`].
///
/// Returns `Ok(None)` if the call is not a supported runtime function (caller should treat as
/// unsupported expression).
pub fn try_eval_runtime_function_call(
    caller: Option<Principal>,
    name: &ObjectName,
    args: &[gleaph_gql::ast::Expr],
    distinct: bool,
) -> Result<Option<Value>, RuntimeFunctionError> {
    let Some(last) = name.parts.last().map(|s| s.as_str()) else {
        return Ok(None);
    };
    if !last.eq_ignore_ascii_case("msg_caller") {
        return Ok(None);
    }
    if name.parts.len() != 1 {
        return Err(RuntimeFunctionError::QualifiedNameNotSupported {
            name: name.parts.join("."),
        });
    }
    if distinct {
        return Err(RuntimeFunctionError::DistinctNotSupported {
            function: "MSG_CALLER",
        });
    }
    if !args.is_empty() {
        return Err(RuntimeFunctionError::InvalidArity {
            function: "MSG_CALLER",
            expected: 0,
            got: args.len(),
        });
    }
    let Some(p) = caller else {
        return Err(RuntimeFunctionError::MissingCallerContext {
            function: "MSG_CALLER",
        });
    };
    Ok(Some(principal_to_value(p)))
}
