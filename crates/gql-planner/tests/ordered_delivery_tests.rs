//! ADR 0081 Slice A: index-ordered ORDER BY delivery.
//!
//! Pins the planner-side eligibility matrix, the wire-visible intent on the vertex
//! scan op, and the explain surface: eligible pipelines record
//! `IndexScan { ordered_by_sort }` and either elide `Sort` (no limit) or keep it for
//! TopK fusion (limit); ineligible pipelines compile exactly as before.

use gleaph_gql::parser;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_planner::explain::explain_plan;
use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
use gleaph_gql_planner::stats::TableStats;
use gleaph_gql_planner::{PlanBuildOptions, build_plan_with_schema_and_options};

fn parse_linear(input: &str) -> gleaph_gql::ast::LinearQueryStatement {
    let program = parser::parse(input).expect("parse");
    let block = program
        .transaction_activity
        .expect("tx activity")
        .body
        .expect("block");
    match &block.first {
        gleaph_gql::ast::Statement::Query(composite) => composite.left.clone(),
        other => panic!("expected query statement, got {other:?}"),
    }
}

fn plan_with_stats(input: &str, stats: &TableStats) -> PhysicalPlan {
    build_plan_with_schema_and_options(
        &parse_linear(input),
        PlanBuildOptions {
            stats: Some(stats),
            path_extensions: &gleaph_gql_planner::RejectingPathExtensionHandler,
        },
        &NoSchema,
    )
    .expect("plan should build")
}

fn indexed_stats() -> TableStats {
    let mut stats = TableStats::default();
    stats.label_cardinality.insert("User".to_string(), 100);
    stats.indexed_vertex_properties.insert("p".into());
    stats.range_indexed_vertex_properties.insert("p".into());
    stats.range_indexed_vertex_properties.insert("name".into());
    stats
}

fn scan_intent(plan: &PhysicalPlan) -> Option<Option<String>> {
    match plan.ops.first() {
        Some(PlanOp::IndexScan {
            ordered_by_sort, ..
        }) => Some(ordered_by_sort.as_ref().map(|p| p.to_string())),
        _ => None,
    }
}

fn has_unconditional_sort(plan: &PhysicalPlan) -> bool {
    plan.ops.iter().any(|op| matches!(op, PlanOp::Sort { .. }))
}

fn has_topk(plan: &PhysicalPlan) -> bool {
    plan.ops.iter().any(|op| matches!(op, PlanOp::TopK { .. }))
}

// ─── Eligible anchors ──────────────────────────────────────────────────────────

#[test]
fn equality_anchor_elides_sort_without_limit() {
    let input = "MATCH (v:User) WHERE v.p = 5 RETURN v ORDER BY v.p";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(Some("p".into())));
    assert!(
        !has_unconditional_sort(&plan),
        "Sort must be elided: {plan:?}"
    );
    assert!(!has_topk(&plan));
}

#[test]
fn range_anchor_keeps_sort_for_topk_when_limit_present() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p LIMIT 3";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(Some("p".into())));
    assert!(has_topk(&plan), "Sort+Limit must fuse into TopK: {plan:?}");
    assert!(!has_unconditional_sort(&plan));
}

#[cfg(feature = "cypher")]
#[test]
fn inlist_anchor_elides_sort() {
    let input = "MATCH (v:User) WHERE v.p IN [1, 2, 3] RETURN v ORDER BY v.p";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(Some("p".into())));
    assert!(!has_unconditional_sort(&plan));
}

#[cfg(feature = "cypher")]
#[test]
fn prefix_anchor_elides_sort() {
    let input = "MATCH (v:User) WHERE v.name STARTS WITH 'A' RETURN v ORDER BY v.name";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(Some("name".into())));
    assert!(!has_unconditional_sort(&plan));
}

// ─── Ineligible shapes keep an unconditional Sort and no intent ────────────────

#[test]
fn desc_direction_stays_ineligible() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p DESC";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(None));
    assert!(has_unconditional_sort(&plan));
}

#[test]
fn multiple_sort_items_stay_ineligible() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p, v.q";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(None));
    assert!(has_unconditional_sort(&plan));
}

#[test]
fn distinct_return_stays_ineligible() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN DISTINCT v ORDER BY v.p";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(None));
    assert!(has_unconditional_sort(&plan));
}

#[test]
fn expression_sort_stays_ineligible() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p + 1";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(None));
    assert!(has_unconditional_sort(&plan));
}

#[test]
fn different_property_sort_stays_ineligible() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.q";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(None));
    assert!(has_unconditional_sort(&plan));
}

#[test]
fn missing_anchor_stays_ineligible() {
    // Without index statistics there is no driving IndexScan: leading NodeScan plus
    // residual filter must keep the unconditional Sort.
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p";
    let plan = plan_with_stats(input, &TableStats::default());
    assert!(
        !matches!(plan.ops.first(), Some(PlanOp::IndexScan { .. })),
        "unexpected index anchor: {plan:?}"
    );
    assert_eq!(scan_intent(&plan), None);
    assert!(has_unconditional_sort(&plan));
}

// ─── Explain pins ───────────────────────────────────────────────────────────────

#[test]
fn explain_shows_ordered_intent_on_eligible_scan() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p LIMIT 20";
    let explained = explain_plan(&plan_with_stats(input, &indexed_stats()));
    assert!(
        explained.contains("[ordered asc by p]"),
        "explain must pin ordered delivery: {explained}"
    );
}

#[test]
fn explain_shows_no_order_marker_for_ineligible_scan() {
    let input = "MATCH (v:User) WHERE v.p >= 10 RETURN v ORDER BY v.p DESC LIMIT 20";
    let explained = explain_plan(&plan_with_stats(input, &indexed_stats()));
    assert!(
        !explained.contains("[ordered"),
        "DESC must not claim ordered delivery: {explained}"
    );
}

// ─── Robustness: parameter/range anchors and subplan boundaries ─────────────────

#[test]
fn parameter_bound_range_anchor_records_intent() {
    let input = "MATCH (v:User) WHERE v.p >= $min RETURN v.p AS p ORDER BY p LIMIT 3";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(
        scan_intent(&plan),
        Some(Some("p".to_string())),
        "a parameterized range anchor is still an eligible leading scan: {plan:?}"
    );
    assert!(!has_unconditional_sort(&plan));
}

#[test]
fn parameter_bound_equality_anchor_records_intent() {
    let input = "MATCH (v:User) WHERE v.p = $id RETURN v.p AS p ORDER BY p LIMIT 3";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_eq!(scan_intent(&plan), Some(Some("p".to_string())));
    assert!(!has_unconditional_sort(&plan));
}

fn plan_has_no_ordered_intent(plan: &PhysicalPlan) -> bool {
    fn walk(ops: &[PlanOp]) -> bool {
        ops.iter().all(|op| match op {
            PlanOp::IndexScan {
                ordered_by_sort, ..
            } => ordered_by_sort.is_none(),
            PlanOp::UseGraph {
                sub_plan: Some(inner),
                ..
            } => walk(inner),
            // Subplans that can host their own pipelines.
            PlanOp::InlineProcedureCall { sub_plan, .. } => walk(&sub_plan.ops),
            PlanOp::HashJoin { left, right, .. } => walk(left) && walk(right),
            _ => true,
        })
    }
    walk(&plan.ops)
}

#[test]
fn use_graph_subplan_does_not_fire_eligibility() {
    let input = "USE otherGraph MATCH (v:User) WHERE v.p >= 1 RETURN v.p AS p ORDER BY p LIMIT 3";
    let plan = plan_with_stats(input, &indexed_stats());
    assert!(
        matches!(plan.ops.first(), Some(PlanOp::UseGraph { .. })),
        "focused statement must lead with UseGraph: {:?}",
        plan.ops
    );
    assert!(
        plan_has_no_ordered_intent(&plan),
        "eligibility must not reach across the UseGraph subplan boundary: {plan:?}"
    );
}

#[test]
fn optional_match_between_scan_and_boundary_blocks_eligibility() {
    let input = "MATCH (v:User) WHERE v.p >= $min OPTIONAL MATCH (v)-[e]->(w) \
                 RETURN v.p AS p ORDER BY p LIMIT 3";
    let plan = plan_with_stats(input, &indexed_stats());
    assert_ne!(
        scan_intent(&plan),
        Some(Some("p".to_string())),
        "OptionalMatch between scan and boundary must veto eligibility: {plan:?}"
    );
}
