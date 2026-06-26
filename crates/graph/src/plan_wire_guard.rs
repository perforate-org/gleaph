//! Defense-in-depth checks for router-delivered [`PhysicalPlan`] wire blobs.

use gleaph_gql_planner::PhysicalPlan;
use gleaph_gql_planner::plan::PlanOp;
use gleaph_graph_kernel::plan_exec::{GqlExecutionMode, SeedBindingsWire};

/// Wire plan validation failure (mapped to [`crate::gql_run::GqlRunError`] at the boundary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanWireGuardError(pub String);

impl std::fmt::Display for PlanWireGuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PlanWireGuardError {}

/// Whether any statement plan in the bundle contains a DML operator.
pub fn bundle_contains_dml(plans: &[PhysicalPlan]) -> bool {
    plans.iter().any(|p| p.has_dml())
}

/// `GPL` bundle `requires_write_path` header must match actual plan operators.
pub fn validate_plan_bundle_write_flag(
    plans: &[PhysicalPlan],
    bundle_requires_write: bool,
) -> Result<(), PlanWireGuardError> {
    let plan_has_dml = bundle_contains_dml(plans);
    if bundle_requires_write != plan_has_dml {
        return Err(PlanWireGuardError(format!(
            "plan bundle write flag ({bundle_requires_write}) does not match plan DML content ({plan_has_dml})"
        )));
    }
    Ok(())
}

/// Query / composite-query execution must not run graph mutations.
pub fn ensure_query_path_has_no_dml(plans: &[PhysicalPlan]) -> Result<(), PlanWireGuardError> {
    if bundle_contains_dml(plans) {
        return Err(PlanWireGuardError(
            "DML operators are not allowed on the query execution path".into(),
        ));
    }
    Ok(())
}

/// Enforce IC execution mode against decoded plans (after bundle flag validation).
pub fn ensure_execution_mode_matches_plans(
    mode: GqlExecutionMode,
    plans: &[PhysicalPlan],
) -> Result<(), PlanWireGuardError> {
    let plan_has_dml = bundle_contains_dml(plans);
    match mode {
        GqlExecutionMode::Query if plan_has_dml => Err(PlanWireGuardError(
            "DML plan cannot run on query path; use execute_plan_update".into(),
        )),
        GqlExecutionMode::Update if !plan_has_dml => Err(PlanWireGuardError(
            "read-only plan cannot run on update path; use execute_plan_query".into(),
        )),
        _ => Ok(()),
    }
}

fn is_leading_seed_skippable_op(op: &PlanOp) -> bool {
    matches!(
        op,
        PlanOp::NodeScan { label: Some(_), .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::ConditionalIndexScan { .. }
            | PlanOp::EdgeIndexScan { .. }
            | PlanOp::PropertyFilter { .. }
    )
}

fn plan_requires_router_seeds(plan: &PhysicalPlan) -> bool {
    plan.ops
        .iter()
        .take_while(|op| is_leading_seed_skippable_op(op))
        .any(|op| {
            matches!(
                op,
                PlanOp::IndexScan { .. }
                    | PlanOp::IndexIntersection { .. }
                    | PlanOp::ConditionalIndexScan { .. }
                    | PlanOp::EdgeIndexScan { .. }
                    | PlanOp::NodeScan { label: Some(_), .. }
            )
        })
}

fn seeds_are_effective(seeds: Option<&SeedBindingsWire>) -> bool {
    seeds.is_some_and(|wire| {
        let grouped_effective = wire.entries.iter().any(|entry| {
            !entry.local_vertex_ids.is_empty() || !entry.local_edge_postings.is_empty()
        });
        grouped_effective || !wire.rows.is_empty()
    })
}

/// Federated graph shards must receive router `seed_bindings_blob` for index-anchor read plans.
pub fn ensure_federated_seeds_for_index_anchors(
    seeds: Option<&SeedBindingsWire>,
    federation_configured: bool,
    plans: &[PhysicalPlan],
) -> Result<(), PlanWireGuardError> {
    if !federation_configured || seeds_are_effective(seeds) {
        return Ok(());
    }
    if plans.iter().any(plan_requires_router_seeds) {
        return Err(PlanWireGuardError(
            "unsupported plan query operator: IndexScan(no index client)".into(),
        ));
    }
    Ok(())
}

/// Full wire-plan gate used by [`crate::gql_run::run_wire_plans`].
pub fn validate_wire_plan_execution(
    mode: GqlExecutionMode,
    plans: &[PhysicalPlan],
    bundle_requires_write: bool,
) -> Result<(), PlanWireGuardError> {
    validate_plan_bundle_write_flag(plans, bundle_requires_write)?;
    match mode {
        GqlExecutionMode::Query => ensure_query_path_has_no_dml(plans)?,
        GqlExecutionMode::Update => {}
    }
    ensure_execution_mode_matches_plans(mode, plans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_planner::plan::PlanOp;

    fn plan_with_dml() -> PhysicalPlan {
        PhysicalPlan {
            ops: vec![PlanOp::DeleteVertex {
                variable: "n".into(),
            }],
            ..Default::default()
        }
    }

    fn read_only_plan() -> PhysicalPlan {
        PhysicalPlan {
            ops: vec![PlanOp::NodeScan {
                variable: "n".into(),
                label: None,
                property_projection: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn rejects_mismatched_bundle_flag() {
        let err =
            validate_plan_bundle_write_flag(&[plan_with_dml()], false).expect_err("flag mismatch");
        assert!(err.0.contains("does not match"));
    }

    #[test]
    fn rejects_dml_on_query_path() {
        let err = validate_wire_plan_execution(GqlExecutionMode::Query, &[plan_with_dml()], true)
            .expect_err("dml on query");
        assert!(err.0.contains("query execution path"));
    }

    #[test]
    fn accepts_read_only_on_query_path() {
        validate_wire_plan_execution(GqlExecutionMode::Query, &[read_only_plan()], false)
            .expect("ok");
    }

    #[test]
    fn rejects_unseeded_federated_index_scan() {
        let plan = PhysicalPlan {
            ops: vec![
                PlanOp::IndexScan {
                    variable: "n".into(),
                    property: "age".into(),
                    value: gleaph_gql_planner::plan::ScanValue::Literal(gleaph_gql::Value::Int64(
                        5,
                    )),
                    cmp: gleaph_gql::ast::CmpOp::Eq,
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![gleaph_gql_planner::plan::ProjectColumn {
                        expr: gleaph_gql::ast::Expr::var("n"),
                        alias: Some("n".into()),
                    }],
                    distinct: false,
                },
            ],
            ..Default::default()
        };
        let err = ensure_federated_seeds_for_index_anchors(None, true, std::slice::from_ref(&plan))
            .expect_err("missing seeds");
        assert!(err.0.contains("IndexScan(no index client)"));
    }

    #[test]
    fn accepts_unseeded_federated_node_scan_without_label() {
        ensure_federated_seeds_for_index_anchors(None, true, &[read_only_plan()]).expect("ok");
    }
}
