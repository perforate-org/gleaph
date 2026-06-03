//! Per-invocation context for GQL execution (canister caller, runtime functions).

use std::fmt;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::ObjectName;
use gleaph_gql_ic::principal_to_value;
use gleaph_graph_kernel::entry::{EdgeLabelId, VertexLabelId};
use gleaph_graph_kernel::plan_exec::ResolvedLabelTable;

/// Carries data that is fixed for one GQL execution (adhoc, prepared, or plan replay).
#[derive(Clone, Debug, Default)]
pub struct GqlExecutionContext {
    /// Internet Computer caller principal when executing on a canister.
    pub caller: Option<Principal>,
    /// Router-resolved label names for this execution.
    pub resolved_labels: Option<ResolvedLabelTable>,
}

impl GqlExecutionContext {
    pub fn resolved_vertex_label_id(&self, name: &str) -> Option<VertexLabelId> {
        self.resolved_labels
            .as_ref()?
            .vertex
            .iter()
            .find(|label| label.name == name)
            .map(|label| label.id)
    }

    pub fn resolved_edge_label_id(&self, name: &str) -> Option<EdgeLabelId> {
        self.resolved_labels
            .as_ref()?
            .edge
            .iter()
            .find(|label| label.name == name)
            .map(|label| label.id)
    }

    pub fn requires_resolved_labels(&self) -> bool {
        self.resolved_labels.is_some()
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
