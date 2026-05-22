//! Defense-in-depth checks for router-delivered [`PhysicalPlan`] wire blobs.

use gleaph_gql_planner::PhysicalPlan;
use gleaph_graph_kernel::plan_exec::GqlExecutionMode;

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
}
