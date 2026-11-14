//! The single owner of "which leading anchor can a Router seed row replace".
//!
//! Router seeds are bounded index/label lookups. An unlabeled full `NodeScan` and a
//! `ConditionalIndexScan` (which may fall back to a full scan) do not pin a bounded anchor and
//! must never be skipped from the plan; the Graph executor then executes them normally.

use gleaph_gql_planner::PlanOp;

/// True for a leading anchor op whose result a Router seed row can represent.
pub(crate) fn is_seed_skippable_anchor(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::NodeScan { label: Some(_), .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::EdgeIndexScan { .. }
    )
}

/// The variable a seed-skippable anchor binds, when it binds exactly one.
pub(crate) fn seeded_anchor_variable(op: &PlanOp) -> Option<&str> {
    match op {
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::IndexIntersection { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. } => Some(variable.as_ref()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql_planner::plan::{NodeLabelRef, ScanValue};

    fn node_scan(label: Option<&str>) -> PlanOp {
        PlanOp::NodeScan {
            variable: "n".into(),
            label: label.map(NodeLabelRef::from),
            property_projection: None,
        }
    }

    fn index_scan() -> PlanOp {
        PlanOp::IndexScan {
            variable: "n".into(),
            property: "name".into(),
            value: ScanValue::Literal(Value::Int64(1)),
            cmp: CmpOp::Eq,
            property_projection: None,
            ordered_by_sort: None,
        }
    }

    #[test]
    fn only_bounded_anchors_are_seed_skippable() {
        assert!(is_seed_skippable_anchor(&node_scan(Some("Person"))));
        assert!(!is_seed_skippable_anchor(&node_scan(None)));
        assert!(is_seed_skippable_anchor(&index_scan()));

        let conditional = PlanOp::ConditionalIndexScan {
            candidates: Vec::new(),
            fallback_label: None,
            fallback_variable: "n".into(),
            property_projection: None,
        };
        assert!(
            !is_seed_skippable_anchor(&conditional),
            "a conditional scan may fall back to a full scan and cannot be replaced by a seed"
        );
    }

    #[test]
    fn seeded_anchor_variable_names_the_bound_variable() {
        assert_eq!(
            seeded_anchor_variable(&node_scan(Some("Person"))),
            Some("n")
        );
        assert_eq!(seeded_anchor_variable(&index_scan()), Some("n"));
        assert_eq!(
            seeded_anchor_variable(&PlanOp::PropertyFilter {
                predicates: Vec::new(),
                stage: 0,
            }),
            None
        );
    }
}
