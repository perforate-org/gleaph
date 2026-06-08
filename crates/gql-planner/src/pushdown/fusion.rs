use crate::plan::{PlanAnnotations, PlanOp};

use super::vars::all_variables_eq;

pub fn apply_ev_fusion(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let mut i = 0;
    while i + 1 < ops.len() {
        let can_fuse = match (&ops[i], &ops[i + 1]) {
            (PlanOp::Expand { dst, .. }, PlanOp::PropertyFilter { predicates, .. }) => {
                // Check all predicates reference only the dst variable (zero-copy).
                predicates.iter().all(|pred| all_variables_eq(pred, dst))
            }
            _ => false,
        };

        if can_fuse {
            // Extract both ops.
            let filter_op = ops.remove(i + 1);
            let expand_op = ops.remove(i);

            if let (
                PlanOp::Expand {
                    src,
                    edge,
                    dst,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    edge_payload_predicate,
                    edge_vector_predicate,
                    edge_property_projection,
                    dst_property_projection,
                    hop_aux_binding,
                    emit_edge_binding,
                    near_group_var: _,
                    far_group_var: _,
                    path_var: _,
                    emit_path_binding: _,
                },
                PlanOp::PropertyFilter { predicates, .. },
            ) = (expand_op, filter_op)
            {
                ops.insert(
                    i,
                    PlanOp::ExpandFilter {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        edge_payload_predicate,
                        edge_vector_predicate,
                        dst_filter: predicates,
                        edge_property_projection,
                        dst_property_projection,
                        hop_aux_binding,
                        emit_edge_binding,
                        near_group_var: None,
                        far_group_var: None,
                        path_var: None,
                        emit_path_binding: false,
                    },
                );
                annotations.optimizer.ev_fusion_applied = true;
            }
            // Don't increment i — check the new ExpandFilter against next op.
        } else {
            i += 1;
        }
    }
}

/// LateProject: ensure Project appears after all Filter/ExpandFilter ops.
/// If Project is found before any filtering op, move it after the last one.
pub fn apply_late_project(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    let project_idx = ops
        .iter()
        .position(|op| matches!(op, PlanOp::Project { .. }));
    let last_filter_idx = ops.iter().rposition(|op| {
        matches!(
            op,
            PlanOp::PropertyFilter { .. }
                | PlanOp::Filter { .. }
                | PlanOp::ExpandFilter { .. }
                | PlanOp::Expand { .. }
        )
    });

    if let (Some(pi), Some(fi)) = (project_idx, last_filter_idx) {
        if pi < fi {
            let project_op = ops.remove(pi);
            // fi shifted by -1 since we removed an element before it.
            ops.insert(fi, project_op);
            annotations.optimizer.late_project_applied = true;
        } else {
            annotations.optimizer.late_project_applied = true; // Already in the right place.
        }
    }
}
