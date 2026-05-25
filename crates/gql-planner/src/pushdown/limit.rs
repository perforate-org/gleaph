use crate::plan::{PlanAnnotations, PlanOp};

pub fn apply_limit_pushdown(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    // Single scan to gather all needed info.
    let mut has_sort = false;
    let mut has_aggregate = false;
    let mut has_distinct = false;
    let mut limit_idx = None;
    let mut project_idx = None;
    for (i, op) in ops.iter().enumerate() {
        match op {
            PlanOp::Sort { .. } => has_sort = true,
            PlanOp::Aggregate { .. } => has_aggregate = true,
            PlanOp::Project { distinct: true, .. } => has_distinct = true,
            PlanOp::Limit { .. } => limit_idx = Some(i),
            PlanOp::Project { .. } => project_idx = Some(i),
            _ => {}
        }
    }

    if has_sort || has_aggregate || has_distinct {
        return;
    }

    if let (Some(li), Some(pi)) = (limit_idx, project_idx)
        && li > pi
    {
        // Move Limit before Project.
        let limit_op = ops.remove(li);
        ops.insert(pi, limit_op);
        annotations.optimizer.limit_pushdown_applied = true;
    }
}

/// TopK fusion: when Sort is immediately followed by Limit (possibly with
/// a Project in between), fuse them into a single TopK operator.
pub fn apply_topk_fusion(ops: &mut Vec<PlanOp>, annotations: &mut PlanAnnotations) {
    // Look for Sort followed by Limit (with optional Project in between).
    let sort_idx = ops.iter().rposition(|op| matches!(op, PlanOp::Sort { .. }));
    let limit_idx = ops
        .iter()
        .rposition(|op| matches!(op, PlanOp::Limit { count: Some(_), .. }));

    if let (Some(si), Some(li)) = (sort_idx, limit_idx) {
        // Limit must come after Sort, and there should be no Aggregate between them.
        if li > si
            && !ops[si..li]
                .iter()
                .any(|op| matches!(op, PlanOp::Aggregate { .. }))
        {
            // Extract Sort and Limit.
            let limit_op = ops.remove(li);
            let sort_op = ops.remove(si);

            if let (
                PlanOp::Sort { order_by },
                PlanOp::Limit {
                    count: Some(k),
                    offset,
                },
            ) = (sort_op, limit_op)
            {
                ops.insert(
                    si,
                    PlanOp::TopK {
                        order_by,
                        k,
                        offset,
                    },
                );
                annotations.optimizer.topk_applied = true;
            }
        }
    }
}
