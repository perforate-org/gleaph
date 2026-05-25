use crate::plan::{PlanAnnotations, PlanOp};

pub fn apply_predicate_reordering(
    ops: &mut [PlanOp],
    annotations: &mut PlanAnnotations,
    stats: Option<&dyn crate::stats::GraphStats>,
) {
    let mut reordered = false;
    for op in ops.iter_mut() {
        if let PlanOp::PropertyFilter { predicates, .. } = op
            && predicates.len() > 1
        {
            // Capture original order by pointer identity.
            let original_ptrs: Vec<*const gleaph_gql::ast::Expr> =
                predicates.iter().map(|p| p as *const _).collect();
            predicates.sort_by(|a, b| {
                let sa = crate::cost::estimate_predicate_selectivity(a, stats);
                let sb = crate::cost::estimate_predicate_selectivity(b, stats);
                sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
            });
            let new_ptrs: Vec<*const gleaph_gql::ast::Expr> =
                predicates.iter().map(|p| p as *const _).collect();
            if original_ptrs != new_ptrs {
                reordered = true;
            }
        }
    }
    if reordered {
        annotations.optimizer.predicate_reordering_applied = true;
    }
}
