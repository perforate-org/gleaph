//! The one owner of a physical plan's nested sub-plan edges.
//!
//! `UseGraph`, `OptionalMatch`, `SemiApply`, `InlineProcedureCall`, set operations and
//! joins all carry nested `PlanOp` slices. Walkers that re-encode this list drift: a newly
//! nested segment becomes invisible to one analyzer but visible to another. Every Router
//! walker now asks this module for the children, and decides the policy for each edge itself
//! (for example, authz deliberately stays blind to `SemiApply` policy machinery).

use gleaph_gql_planner::PlanOp;

/// Calls `f` for every direct nested sub-plan slice of `op`.
pub(crate) fn for_each_child<'a>(op: &'a PlanOp, mut f: impl FnMut(&'a [PlanOp])) {
    match op {
        PlanOp::UseGraph {
            sub_plan: Some(sub),
            ..
        } => f(sub),
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            f(left);
            f(right);
        }
        PlanOp::SetOperation { right, .. } => f(right.ops()),
        PlanOp::OptionalMatch { sub_plan } => f(sub_plan),
        PlanOp::InlineProcedureCall { sub_plan, .. } => f(sub_plan.ops()),
        PlanOp::SemiApply { sub_plan, .. } => f(sub_plan),
        _ => {}
    }
}

/// Mutable counterpart of [`for_each_child`], for lowering passes.
pub(crate) fn for_each_child_mut(op: &mut PlanOp, mut f: impl FnMut(&mut Vec<PlanOp>)) {
    match op {
        PlanOp::UseGraph {
            sub_plan: Some(sub),
            ..
        } => f(sub),
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            f(left);
            f(right);
        }
        PlanOp::SetOperation { right, .. } => f(right.ops_mut()),
        PlanOp::OptionalMatch { sub_plan } => f(sub_plan),
        PlanOp::InlineProcedureCall { sub_plan, .. } => f(sub_plan.ops_mut()),
        PlanOp::SemiApply { sub_plan, .. } => f(sub_plan),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child_of(op: &PlanOp) -> Vec<usize> {
        let mut lengths = Vec::new();
        for_each_child(op, |child| lengths.push(child.len()));
        lengths
    }

    #[test]
    fn every_nested_plan_edge_is_enumerated() {
        let leaf = PlanOp::Filter {
            condition: gleaph_gql::ast::Expr::var("x"),
        };
        assert_eq!(
            child_of(&PlanOp::UseGraph {
                graph_name: vec!["g".into()],
                sub_plan: Some(vec![leaf.clone()]),
            }),
            vec![1]
        );
        assert_eq!(
            child_of(&PlanOp::OptionalMatch {
                sub_plan: vec![leaf.clone()],
            }),
            vec![1]
        );
        assert_eq!(
            child_of(&PlanOp::SemiApply {
                source: "x".into(),
                hops: Vec::new(),
                terminal_predicates: Vec::new(),
                sub_plan: vec![leaf.clone()],
            }),
            vec![1]
        );
        assert_eq!(
            child_of(&PlanOp::CartesianProduct {
                left: vec![leaf.clone()],
                right: vec![leaf.clone(), leaf.clone()],
            }),
            vec![1, 2]
        );
    }
}
