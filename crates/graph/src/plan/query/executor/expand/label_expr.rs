//! Edge label expression matching for `Expand` (`-/A|B/->`).

use gleaph_gql::types::{LabelExpr, matches_edge_label};
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::BucketLabelKey as LaraLabelId;

use super::super::bindings::EdgeBinding;
use crate::facade::catalog_edge_label_from_wire;
use crate::gql_execution_context::GqlExecutionContext;

pub(crate) fn edge_wire_label_matches_label_expr(
    execution: &GqlExecutionContext,
    label_expr: &LabelExpr,
    wire_label_id: LaraLabelId,
) -> bool {
    let catalog_id = catalog_edge_label_from_wire(wire_label_id);
    let name = catalog_id.and_then(|id| execution.resolved_edge_label_name(id));
    matches_edge_label(label_expr, name.as_deref())
}

pub(crate) fn edge_binding_matches_label_expr(
    execution: &GqlExecutionContext,
    label_expr: &LabelExpr,
    binding: &EdgeBinding,
) -> bool {
    edge_wire_label_matches_label_expr(execution, label_expr, binding.handle.label_id)
}

pub(crate) fn edge_matches_label_expr(
    execution: &GqlExecutionContext,
    label_expr: &LabelExpr,
    edge: &Edge,
) -> bool {
    edge_wire_label_matches_label_expr(execution, label_expr, LaraLabelId::from_raw(edge.label_id))
}
