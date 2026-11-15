//! The single owner of "which leading anchor can a Router seed row replace".
//!
//! Router seeds are bounded index/label lookups. An unlabeled full `NodeScan` and a
//! `ConditionalIndexScan` (which may fall back to a full scan) do not pin a bounded anchor and
//! must never be skipped from the plan; the Graph executor then executes them normally.

use std::borrow::Cow;

use gleaph_gql_planner::PlanOp;

use super::query::PlanRow;

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

/// A read plan's ops paired with the rows they start from.
///
/// The only way to run a read plan from supplied rows. Ordinary rows run every op; Router seed rows
/// (ADR 0046) stand for the leading anchors whose variable every row binds, so those anchors are
/// removed once here - at the head of the plan and, recursively, at the head of each arm of a
/// leading `CartesianProduct` (disconnected `MATCH` patterns, nested left-deep for three or more).
/// The removed anchors are kept so the caller revalidates exactly what was removed.
///
/// Removal is an optimization, not a correctness rule: an anchor left in place runs over rows that
/// already bind its variable, and [`PlanRow::bind`] keeps only the rows whose binding it confirms.
pub struct SeededPlan<'a> {
    ops: Cow<'a, [PlanOp]>,
    replaced: Vec<&'a PlanOp>,
    rows: Vec<PlanRow>,
}

impl<'a> SeededPlan<'a> {
    /// Rows that every op runs over (a fresh empty row, correlated outer rows).
    pub fn plain(ops: &'a [PlanOp], rows: Vec<PlanRow>) -> Self {
        Self {
            ops: Cow::Borrowed(ops),
            replaced: Vec::new(),
            rows,
        }
    }

    /// Router seed rows: the leading seedable anchors they bind are replaced by the rows.
    ///
    /// An explicitly empty seed relation binds every variable vacuously, so every leading anchor
    /// is replaced and the plan yields what the residual ops make of zero rows.
    pub fn router_seed(ops: &'a [PlanOp], rows: Vec<PlanRow>) -> Self {
        let mut replaced = Vec::new();
        let ops = without_seeded_anchors(ops, &rows, &mut replaced);
        Self {
            ops,
            replaced,
            rows,
        }
    }

    /// The anchors the seed rows replaced, in plan order.
    pub fn replaced_anchors(&self) -> &[&'a PlanOp] {
        &self.replaced
    }

    pub fn ops(&self) -> &[PlanOp] {
        &self.ops
    }

    /// The ops to run and the rows to start them from.
    pub fn into_parts(self) -> (Cow<'a, [PlanOp]>, Vec<PlanRow>) {
        (self.ops, self.rows)
    }
}

fn without_seeded_anchors<'a>(
    ops: &'a [PlanOp],
    rows: &[PlanRow],
    replaced: &mut Vec<&'a PlanOp>,
) -> Cow<'a, [PlanOp]> {
    let seeded = |op: &PlanOp| {
        is_seed_skippable_anchor(op)
            && seeded_anchor_variable(op)
                .is_some_and(|name| rows.iter().all(|row| row.contains_key(name)))
    };
    let skip = ops.iter().take_while(|op| seeded(op)).count();
    replaced.extend(&ops[..skip]);
    let rest = &ops[skip..];
    let Some((PlanOp::CartesianProduct { left, right }, tail)) = rest.split_first() else {
        return Cow::Borrowed(rest);
    };
    let left_ops = without_seeded_anchors(left, rows, replaced);
    let right_ops = without_seeded_anchors(right, rows, replaced);
    // A borrowed result of full length is the arm itself; anything else replaced an anchor.
    let unchanged = |arm: &Cow<'_, [PlanOp]>, original: &[PlanOp]| matches!(arm, Cow::Borrowed(ops) if ops.len() == original.len());
    if unchanged(&left_ops, left) && unchanged(&right_ops, right) {
        return Cow::Borrowed(rest);
    }
    let mut owned = Vec::with_capacity(rest.len());
    owned.push(PlanOp::CartesianProduct {
        left: left_ops.into_owned(),
        right: right_ops.into_owned(),
    });
    owned.extend_from_slice(tail);
    Cow::Owned(owned)
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

    fn labeled(variable: &str) -> PlanOp {
        PlanOp::NodeScan {
            variable: variable.into(),
            label: Some(NodeLabelRef::from("L")),
            property_projection: None,
        }
    }

    fn product(left: Vec<PlanOp>, right: Vec<PlanOp>) -> PlanOp {
        PlanOp::CartesianProduct { left, right }
    }

    fn row_binding(names: &[&str]) -> PlanRow {
        let mut row = PlanRow::new();
        for (i, name) in names.iter().enumerate() {
            row.insert(
                (*name).to_owned(),
                crate::plan::PlanBinding::Vertex(ic_stable_lara::VertexId::from(i as u32)),
            );
        }
        row
    }

    /// `MATCH (g0:L), (g1:L), (g2:L)`, nested left-deep.
    fn three_patterns() -> Vec<PlanOp> {
        vec![product(
            vec![product(vec![labeled("g0")], vec![labeled("g1")])],
            vec![labeled("g2")],
        )]
    }

    #[test]
    fn router_seed_replaces_an_anchor_nested_in_a_left_deep_product() {
        let ops = three_patterns();
        let seeded = SeededPlan::router_seed(&ops, vec![row_binding(&["g0"])]);
        assert_eq!(
            format!("{:?}", seeded.replaced_anchors()),
            format!("{:?}", [&labeled("g0")])
        );
        assert_eq!(
            format!("{:?}", seeded.ops()),
            format!(
                "{:?}",
                [product(
                    vec![product(Vec::new(), vec![labeled("g1")])],
                    vec![labeled("g2")],
                )]
            )
        );
    }

    #[test]
    fn router_seed_replaces_anchors_in_every_arm_the_rows_bind() {
        let ops = three_patterns();
        let seeded = SeededPlan::router_seed(&ops, vec![row_binding(&["g0", "g1", "g2"])]);
        assert_eq!(
            format!("{:?}", seeded.replaced_anchors()),
            format!("{:?}", [&labeled("g0"), &labeled("g1"), &labeled("g2")])
        );
        assert_eq!(
            format!("{:?}", seeded.ops()),
            format!(
                "{:?}",
                [product(vec![product(Vec::new(), Vec::new())], Vec::new())]
            )
        );
    }

    #[test]
    fn router_seed_keeps_an_anchor_some_row_leaves_unbound() {
        let ops = vec![labeled("g0"), labeled("g1")];
        let seeded =
            SeededPlan::router_seed(&ops, vec![row_binding(&["g0", "g1"]), row_binding(&["g0"])]);
        assert_eq!(
            format!("{:?}", seeded.replaced_anchors()),
            format!("{:?}", [&labeled("g0")])
        );
        assert_eq!(
            format!("{:?}", seeded.ops()),
            format!("{:?}", [labeled("g1")])
        );
    }

    #[test]
    fn plain_rows_replace_nothing() {
        let ops = three_patterns();
        let seeded = SeededPlan::plain(&ops, vec![row_binding(&["g0", "g1", "g2"])]);
        assert!(seeded.replaced_anchors().is_empty());
        assert_eq!(
            format!("{:?}", seeded.ops()),
            format!("{:?}", ops.as_slice())
        );
    }

    #[test]
    fn an_empty_router_seed_replaces_every_leading_anchor() {
        let ops = three_patterns();
        let seeded = SeededPlan::router_seed(&ops, Vec::new());
        assert_eq!(
            format!("{:?}", seeded.replaced_anchors().len()),
            format!("{:?}", 3)
        );
    }
}
