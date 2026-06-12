//! Edge label expression matching for `Expand` (`-/A|B/->`).

use std::collections::BTreeSet;

use gleaph_gql::types::{LabelExpr, matches_edge_label};
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId};
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

/// Distinct catalog edge labels named in `expr` when per-label index/payload/vector fusion applies.
///
/// Returns `None` for wildcards, negation, or when no resolvable label names are present.
pub(crate) fn fusion_edge_label_ids_for_expr(
    execution: &GqlExecutionContext,
    expr: &LabelExpr,
) -> Option<Vec<EdgeLabelId>> {
    if !label_expr_supports_per_label_fusion(expr) {
        return None;
    }
    let mut names = BTreeSet::new();
    collect_edge_label_names_in_expr(expr, &mut names);
    if names.is_empty() {
        return None;
    }
    let mut ids = Vec::with_capacity(names.len());
    for name in names {
        ids.push(execution.resolved_edge_label_id(&name)?);
    }
    Some(ids)
}

/// Edge label ids to try for index/payload/vector fusion when `label_expr` cannot decompose
/// to explicit names (wildcard, negation, etc.).
pub(crate) fn catalog_edge_label_ids_for_predicate_fusion(
    execution: &GqlExecutionContext,
) -> Vec<EdgeLabelId> {
    crate::edge_payload_schema::edge_label_ids_for_predicate_fusion(
        execution.resolved_labels.as_ref(),
    )
}

fn label_expr_supports_per_label_fusion(expr: &LabelExpr) -> bool {
    match expr {
        LabelExpr::Wildcard | LabelExpr::Not(_) => false,
        LabelExpr::Name(_) => true,
        LabelExpr::And(left, right) | LabelExpr::Or(left, right) => {
            label_expr_supports_per_label_fusion(left)
                && label_expr_supports_per_label_fusion(right)
        }
    }
}

fn collect_edge_label_names_in_expr(expr: &LabelExpr, out: &mut BTreeSet<String>) {
    match expr {
        LabelExpr::Name(name) => {
            out.insert(name.clone());
        }
        LabelExpr::And(left, right) | LabelExpr::Or(left, right) => {
            collect_edge_label_names_in_expr(left, out);
            collect_edge_label_names_in_expr(right, out);
        }
        LabelExpr::Not(_) | LabelExpr::Wildcard => {}
    }
}
