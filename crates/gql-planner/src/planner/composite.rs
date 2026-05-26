use gleaph_gql::ast::CompositeQueryExpr;
use gleaph_gql::type_check::{
    BindingKind, NoSchema, PropertySchema,
    infer_composite_query_binding_kinds_and_warnings_with_schema,
};
use std::collections::BTreeMap;

use crate::path_extensions::{PlanBuildOptions, REJECTING_PATH_EXTENSION_HANDLER};
use crate::plan::{PhysicalPlan, PlanOp};
use crate::stats::GraphStats;
use super::{
    build_plan_core, build_plan_with_binding_kinds, build_plan_with_binding_kinds_and_options,
    build_plan_with_schema, build_plan_with_schema_and_options, PlanBuildOutput, PlannerError,
};
use super::validate::{apply_type_checker_dml_diagnostics, validate_plan};
pub fn build_composite_plan(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
) -> Result<PhysicalPlan, PlannerError> {
    build_composite_plan_with_schema(composite, stats, &NoSchema)
}

pub fn build_composite_plan_with_schema_and_options(
    composite: &CompositeQueryExpr,
    options: PlanBuildOptions<'_>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    if composite.rest.is_empty() {
        return build_plan_with_schema_and_options(&composite.left, options, schema);
    }

    let (branch_kinds, type_warnings) =
        infer_composite_query_binding_kinds_and_warnings_with_schema(composite, schema);
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_composite_plan_from_branch_kinds_and_options(
        composite,
        options,
        &branch_kinds,
        schema,
    )?;
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_warnings);
    validate_plan(plan)
}

pub fn build_composite_plan_with_schema(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    if composite.rest.is_empty() {
        return build_plan_with_schema(&composite.left, stats, schema);
    }

    let (branch_kinds, type_warnings) =
        infer_composite_query_binding_kinds_and_warnings_with_schema(composite, schema);
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_composite_plan_from_branch_kinds(composite, stats, &branch_kinds, schema)?;
    apply_type_checker_dml_diagnostics(&mut plan.diagnostics, &type_warnings);
    validate_plan(plan)
}

pub fn build_composite_plan_output(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, PlannerError> {
    build_composite_plan(composite, stats).map(PlanBuildOutput::from_plan)
}

pub fn build_composite_plan_output_with_schema(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    schema: &dyn PropertySchema,
) -> Result<PlanBuildOutput, PlannerError> {
    build_composite_plan_with_schema(composite, stats, schema).map(PlanBuildOutput::from_plan)
}

fn build_composite_plan_from_branch_kinds_and_options(
    composite: &CompositeQueryExpr,
    options: PlanBuildOptions<'_>,
    branch_kinds: &[BTreeMap<String, BindingKind>],
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_plan_core(&composite.left, &branch_kinds[0], schema, options)?;

    for (i, (set_op, right_query)) in composite.rest.iter().enumerate() {
        let right_plan = build_plan_core(right_query, &branch_kinds[1 + i], schema, options)?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

fn build_composite_plan_from_branch_kinds(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    branch_kinds: &[BTreeMap<String, BindingKind>],
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    debug_assert_eq!(branch_kinds.len(), 1 + composite.rest.len());

    let mut plan = build_plan_core(
        &composite.left,
        &branch_kinds[0],
        schema,
        PlanBuildOptions {
            stats,
            path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
        },
    )?;

    for (i, (set_op, right_query)) in composite.rest.iter().enumerate() {
        let right_plan = build_plan_core(
            right_query,
            &branch_kinds[1 + i],
            schema,
            PlanBuildOptions {
                stats,
                path_extensions: &REJECTING_PATH_EXTENSION_HANDLER,
            },
        )?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

pub(crate) fn build_composite_plan_with_binding_kinds_and_options(
    composite: &CompositeQueryExpr,
    options: PlanBuildOptions<'_>,
    seed_binding_kinds: Option<&BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan = build_plan_with_binding_kinds_and_options(
        &composite.left,
        options,
        seed_binding_kinds,
        schema,
    )?;

    for (set_op, right_query) in &composite.rest {
        let right_plan = build_plan_with_binding_kinds_and_options(
            right_query,
            options,
            seed_binding_kinds,
            schema,
        )?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}

pub(crate) fn build_composite_plan_with_binding_kinds(
    composite: &CompositeQueryExpr,
    stats: Option<&dyn GraphStats>,
    seed_binding_kinds: Option<&BTreeMap<String, BindingKind>>,
    schema: &dyn PropertySchema,
) -> Result<PhysicalPlan, PlannerError> {
    let mut plan =
        build_plan_with_binding_kinds(&composite.left, stats, seed_binding_kinds, schema)?;

    // Append set operations (UNION, EXCEPT, INTERSECT, OTHERWISE).
    for (set_op, right_query) in &composite.rest {
        let right_plan =
            build_plan_with_binding_kinds(right_query, stats, seed_binding_kinds, schema)?;
        plan.ops.push(PlanOp::SetOperation {
            op: *set_op,
            right: Box::new(right_plan),
        });
    }

    Ok(plan)
}
