//! Conservative classifier for the typed V1 bulk execution path (ADR 0047).
//!
//! This module lives in `gleaph-gql-integration` because it depends on both the generic GQL
//! planner (for physical-plan shape) and `gleaph-graph-kernel` (for wire types and payload
//! constants). It is pure: it does not call canisters or read durable state.
//!
//! The classifier is intentionally fail-closed. A group is eligible for the typed V1 path only when
//! every operation in the group meets all of the following:
//!
//! - update mode (the new Graph method is an update endpoint);
//! - single target shard across the whole group;
//! - a required complete-row seed relation with no legacy grouped `entries`;
//! - no resolved-search relation;
//! - no uniqueness, constrained, local-unique, or indexed-embedding dispatch state;
//! - a physical plan that requires the write path and is a single-anchor threaded bundle so the
//!   number of hot-forward vertices is bounded by the plan structure. Graph update execution never
//!   materializes `rows_blob`, even when a plan carries output metadata.
//!
//! Anything else keeps the existing scalar or legacy-batch path.

use candid::{Decode, Encode};
use gleaph_gql_planner::wire::decode_plan_bundle;
use gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES;
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanArgs, ExecutePlanBatchMode, ExecutePlanBatchResult, ExecutePlanBatchTypedArgs,
    ExecutePlanBatchTypedShared, ExecutePlanResult, ExecutePlanTypedOp, GqlExecutionMode,
    MAX_TYPED_BATCH_ERROR_BYTES, ResolvedLabelTable, ResolvedPropertyTable, SeedBindingsWire,
};
use gleaph_graph_kernel::vector_index::IndexedEmbeddingCatalog;

/// A homogeneous group of operations that may use the typed V1 bulk envelope.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedBatchCandidate {
    /// Common target shard for every operation.
    pub target_shard_id: gleaph_graph_kernel::federation::ShardId,
    /// Common element-id encoding key.
    pub element_id_encoding_key: [u8; 16],
    /// Shared mutation id for the bulk group.
    pub mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    /// Shared physical plan blob.
    pub plan_blob: Vec<u8>,
    /// Per-operation params and typed seeds, in public item order.
    pub operations: Vec<TypedBatchCandidateOp>,
}

/// One operation inside a typed V1 candidate group.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedBatchCandidateOp {
    pub params_blob: Vec<u8>,
    pub seed: SeedBindingsWire,
}

/// Outcome of classifying a group of `ExecutePlanArgs`.
#[derive(Clone, Debug, PartialEq)]
pub enum TypedBatchEligibility {
    /// Group is eligible. The inner candidate contains the normalized shared/per-op data.
    Eligible(TypedBatchCandidate),
    /// Group must use the existing scalar or legacy batch path.
    Ineligible { reason: &'static str },
}

/// Maximum number of [`PlanOp::InsertEdge`] operators allowed in a typed V1 plan.
///
/// A single-anchor threaded bundle already bounds existing-state reads; this cap additionally
/// bounds the distinct source variables that can become hot-forward vertices, keeping the
/// Graph→Router response size predictable.
const MAX_TYPED_BATCH_INSERT_EDGE_OPS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypedBatchPlanBounds {
    insert_edges_per_input_row: usize,
}

/// Validate the shared physical plan for the typed V1 path.
///
/// Checks:
/// - the blob is a decodable plan bundle;
/// - the bundle requires the write path;
/// - update execution produces no `rows_blob`; output metadata therefore does not affect the wire;
/// - the plan is a single-anchor threaded bundle, so all graph reads come from the seeded anchor;
/// - the number of `InsertEdge` operators is bounded.
///
/// This function is exported so the Graph canister can re-validate the plan before executing
/// the typed endpoint, independent of any Router-side decision.
fn validate_typed_batch_plan(plan_blob: &[u8]) -> Result<TypedBatchPlanBounds, &'static str> {
    let (requires_write_path, plans) =
        decode_plan_bundle(plan_blob).map_err(|_| "typed V1 requires a decodable plan bundle")?;
    if !requires_write_path {
        return Err("typed V1 requires a write-path plan");
    }
    if plans.is_empty() {
        return Err("typed V1 requires at least one physical plan");
    }
    let mut insert_edges_per_input_row = 0usize;
    for plan in &plans {
        if !plan.is_single_anchor_threaded_bundle() {
            return Err("typed V1 requires a single-anchor threaded bundle");
        }
        let plan_insert_edges = count_row_preserving_insert_edges(&plan.ops)?;
        insert_edges_per_input_row = insert_edges_per_input_row
            .checked_add(plan_insert_edges)
            .ok_or("typed V1 edge-insert bound overflow")?;
        if insert_edges_per_input_row > MAX_TYPED_BATCH_INSERT_EDGE_OPS {
            return Err("typed V1 plan exceeds the allowed number of edge inserts");
        }
    }
    Ok(TypedBatchPlanBounds {
        insert_edges_per_input_row,
    })
}

/// Count edge inserts while rejecting every operator that can expand or independently source rows.
///
/// This match is deliberately exhaustive: adding a planner operator forces an explicit typed V1
/// decision instead of silently widening the response bound.
fn count_row_preserving_insert_edges(
    ops: &[gleaph_gql_planner::plan::PlanOp],
) -> Result<usize, &'static str> {
    use gleaph_gql_planner::plan::PlanOp;
    let mut count = 0usize;
    for (index, op) in ops.iter().enumerate() {
        match op {
            PlanOp::NodeScan { .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::EdgeIndexScan { .. }
            | PlanOp::IndexIntersection { .. }
                if index == 0 => {}
            PlanOp::PropertyFilter { .. }
            | PlanOp::Filter { .. }
            | PlanOp::Let { .. }
            | PlanOp::Project { .. }
            | PlanOp::Sort { .. }
            | PlanOp::Limit { .. }
            | PlanOp::TopK { .. }
            | PlanOp::Materialize { .. }
            | PlanOp::InsertVertex { .. }
            | PlanOp::SetProperties { .. }
            | PlanOp::RemoveProperties { .. }
            | PlanOp::DeleteVertex { .. }
            | PlanOp::DetachDeleteVertex { .. }
            | PlanOp::DeleteEdge { .. } => {}
            PlanOp::InsertEdge { .. } => {
                count = count
                    .checked_add(1)
                    .ok_or("typed V1 edge-insert bound overflow")?;
            }
            PlanOp::For { .. } | PlanOp::Aggregate { .. } => {
                return Err("typed V1 does not support row-cardinality-changing operators");
            }
            PlanOp::NodeScan { .. }
            | PlanOp::IndexScan { .. }
            | PlanOp::EdgeIndexScan { .. }
            | PlanOp::IndexIntersection { .. }
            | PlanOp::EdgeBindEndpoints { .. }
            | PlanOp::ConditionalIndexScan { .. }
            | PlanOp::Expand { .. }
            | PlanOp::ExpandFilter { .. }
            | PlanOp::ShortestPath { .. }
            | PlanOp::Search { .. }
            | PlanOp::CallProcedure { .. }
            | PlanOp::InlineProcedureCall { .. }
            | PlanOp::UseGraph { .. }
            | PlanOp::HashJoin { .. }
            | PlanOp::CartesianProduct { .. }
            | PlanOp::SetOperation { .. }
            | PlanOp::OptionalMatch { .. }
            | PlanOp::WorstCaseOptimalJoin { .. } => {
                return Err("typed V1 plan contains an unsupported operator");
            }
        }
    }
    Ok(count)
}

fn validate_typed_batch_response_bound(
    seed_row_counts: impl IntoIterator<Item = usize>,
    plan_bounds: TypedBatchPlanBounds,
) -> Result<(), &'static str> {
    let mut max_hot_vertices_per_op = Vec::new();
    let mut total_hot_vertices = 0usize;
    for rows in seed_row_counts {
        let max_hot_vertices = rows
            .checked_mul(plan_bounds.insert_edges_per_input_row)
            .ok_or("typed V1 response bound overflow")?;
        total_hot_vertices = total_hot_vertices
            .checked_add(max_hot_vertices)
            .ok_or("typed V1 response bound overflow")?;
        max_hot_vertices_per_op.push(max_hot_vertices);
    }

    // Avoid allocating a proof object that already exceeds the payload limit on raw nat32 bytes.
    if total_hot_vertices
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or("typed V1 response bound overflow")?
        > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
    {
        return Err("typed V1 worst-case response exceeds the safe payload limit");
    }

    let mut results = Vec::with_capacity(max_hot_vertices_per_op.len() + 1);
    for max_hot_vertices in max_hot_vertices_per_op {
        results.push(Ok(ExecutePlanResult {
            row_count: u64::MAX,
            rows_blob: None,
            hot_forward_vertices: vec![u32::MAX; max_hot_vertices],
        }));
    }
    // Over-approximate partial failure by appending one maximum bounded error after all successful
    // results. A real batch stops at its first error and therefore cannot contain this extra item.
    results.push(Err("x".repeat(MAX_TYPED_BATCH_ERROR_BYTES)));
    let proof = ExecutePlanBatchResult {
        results,
        next_index: Some(u32::MAX),
    };
    let encoded = Encode!(&proof).map_err(|_| "typed V1 response-bound encode failed")?;
    if encoded.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err("typed V1 worst-case response exceeds the safe payload limit");
    }
    Ok(())
}

/// Decide whether a sequence of `ExecutePlanArgs` can use the typed V1 bulk envelope.
///
/// The slice must be non-empty and all operations must already be homogeneous in plan, mode,
/// target shard, and catalog state. This is normally called after the Router has built the legacy
/// per-operation envelopes for a coalesced bulk group.
pub fn classify_typed_batch_eligibility(operations: &[ExecutePlanArgs]) -> TypedBatchEligibility {
    if operations.is_empty() {
        return TypedBatchEligibility::Ineligible {
            reason: "empty operation list",
        };
    }
    let first = &operations[0];

    // Update mode only: the new Graph endpoint is an update method.
    if first.mode != GqlExecutionMode::Update {
        return TypedBatchEligibility::Ineligible {
            reason: "typed V1 is update-only",
        };
    }

    // Single target shard across the whole group.
    let target_shard_id = first.target_shard_id;
    if operations
        .iter()
        .any(|op| op.target_shard_id != target_shard_id)
    {
        return TypedBatchEligibility::Ineligible {
            reason: "typed V1 requires a single target shard",
        };
    }

    // Shared identity fields must match.
    let element_id_encoding_key = first.element_id_encoding_key;
    let mutation_id = first.mutation_id.unwrap_or(0);
    if operations
        .iter()
        .any(|op| op.element_id_encoding_key != element_id_encoding_key)
    {
        return TypedBatchEligibility::Ineligible {
            reason: "typed V1 requires a shared element_id_encoding_key",
        };
    }
    if operations
        .iter()
        .any(|op| op.mutation_id != Some(mutation_id))
    {
        return TypedBatchEligibility::Ineligible {
            reason: "typed V1 requires a shared mutation_id",
        };
    }

    // All operations must share the same plan blob; the typed envelope carries exactly one plan.
    if operations.iter().any(|op| op.plan_blob != first.plan_blob) {
        return TypedBatchEligibility::Ineligible {
            reason: "typed V1 requires a shared plan_blob",
        };
    }

    // Validate the shared physical plan shape.
    let plan_bounds = match validate_typed_batch_plan(&first.plan_blob) {
        Ok(bounds) => bounds,
        Err(reason) => return TypedBatchEligibility::Ineligible { reason },
    };

    // All catalogs and dispatch state must be identical and empty where forbidden.
    let indexed_properties = first.indexed_properties.clone();
    let resolved_labels = first.resolved_labels.clone();
    let resolved_properties = first.resolved_properties.clone();
    for op in operations.iter() {
        if op.indexed_properties != indexed_properties {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 requires identical indexed_properties across operations",
            };
        }
        if op.resolved_labels != resolved_labels {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 requires identical resolved_labels across operations",
            };
        }
        if op.resolved_properties != resolved_properties {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 requires identical resolved_properties across operations",
            };
        }
        if !is_empty_optional_slice(op.unique_claims.as_ref()) {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 does not support uniqueness claims",
            };
        }
        if !is_empty_optional_slice(op.constrained_properties.as_ref()) {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 does not support constrained properties",
            };
        }
        if !is_empty_optional_slice(op.local_unique_claims.as_ref()) {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 does not support local-unique claims",
            };
        }
        if !is_empty_optional_slice(op.local_constrained_properties.as_ref()) {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 does not support local-constrained properties",
            };
        }
        if op.indexed_embeddings.as_ref() != Some(&IndexedEmbeddingCatalog::default())
            && op.indexed_embeddings.is_some()
        {
            // The default empty catalog is allowed; any non-default catalog is forbidden.
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 does not support indexed-embedding dispatch",
            };
        }
        if op.resolved_search_blob.is_some() {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 does not support resolved search",
            };
        }
    }

    // Build the normalized candidate, validating seed shape along the way.
    let mut candidate_operations = Vec::with_capacity(operations.len());
    for op in operations.iter() {
        let Some(seed) = op
            .seed_bindings_blob
            .as_ref()
            .and_then(|blob| Decode!(blob, SeedBindingsWire).ok())
        else {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 requires a decodable seed_bindings_blob",
            };
        };
        if !seed.entries.is_empty() {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 requires empty grouped seed entries",
            };
        }
        if !seed.complete_prefix_rows {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 requires complete_prefix_rows=true",
            };
        }
        if seed.rows.len() > 1024 {
            return TypedBatchEligibility::Ineligible {
                reason: "typed V1 supports at most 1024 seed rows per operation",
            };
        }
        candidate_operations.push(TypedBatchCandidateOp {
            params_blob: op.params_blob.clone(),
            seed,
        });
    }

    if let Err(reason) = validate_typed_batch_response_bound(
        candidate_operations.iter().map(|op| op.seed.rows.len()),
        plan_bounds,
    ) {
        return TypedBatchEligibility::Ineligible { reason };
    }

    TypedBatchEligibility::Eligible(TypedBatchCandidate {
        target_shard_id,
        element_id_encoding_key,
        mutation_id,
        plan_blob: first.plan_blob.clone(),
        operations: candidate_operations,
    })
}

/// Validate an already-constructed typed batch candidate.
///
/// This is the Router-side production entry point: complete-row seeds have already been resolved
/// as [`SeedBindingsWire`], so the candidate carries them directly without per-operation Candid
/// encoding. It reuses the same plan and response-bound rules as the legacy-args classifier.
pub fn classify_typed_batch_candidate(
    candidate: &TypedBatchCandidate,
    indexed_embeddings: &IndexedEmbeddingCatalog,
) -> Result<(), &'static str> {
    if !indexed_embeddings.is_empty() {
        return Err("typed V1 does not support indexed-embedding dispatch");
    }
    if candidate.operations.is_empty() || candidate.operations.len() > 1024 {
        return Err("typed V1 operation count must be 1..=1024");
    }
    let plan_bounds = validate_typed_batch_plan(&candidate.plan_blob)?;
    for op in &candidate.operations {
        if !op.seed.entries.is_empty() {
            return Err("typed V1 requires empty grouped seed entries");
        }
        if !op.seed.complete_prefix_rows {
            return Err("typed V1 requires complete_prefix_rows=true");
        }
        if op.seed.rows.len() > 1024 {
            return Err("typed V1 supports at most 1024 seed rows per operation");
        }
        if op.params_blob.len() > MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
            return Err("typed V1 params_blob exceeds the safe payload limit");
        }
    }
    validate_typed_batch_response_bound(
        candidate.operations.iter().map(|op| op.seed.rows.len()),
        plan_bounds,
    )?;
    Ok(())
}

/// Build and validate the exact production typed request that crosses the Router→Graph boundary.
///
/// Keeping construction here makes the semantic classifier, full-request size validation, Router
/// admission, and canbench probe share one implementation. Callers must complete this check before
/// persisting a durable typed replay payload; any rejection is a pre-dispatch fallback decision.
pub fn build_and_validate_typed_batch_args(
    candidate: &TypedBatchCandidate,
    resolved_labels: Option<ResolvedLabelTable>,
    resolved_properties: Option<ResolvedPropertyTable>,
    indexed_properties: Option<gleaph_graph_kernel::index::IndexedPropertyCatalog>,
    indexed_embeddings: &IndexedEmbeddingCatalog,
    batch_mode: ExecutePlanBatchMode,
) -> Result<ExecutePlanBatchTypedArgs, String> {
    classify_typed_batch_candidate(candidate, indexed_embeddings).map_err(str::to_string)?;
    let args = ExecutePlanBatchTypedArgs {
        shared: ExecutePlanBatchTypedShared {
            target_shard_id: candidate.target_shard_id,
            element_id_encoding_key: candidate.element_id_encoding_key,
            mutation_id: candidate.mutation_id,
            plan_blob: candidate.plan_blob.clone(),
            resolved_labels,
            resolved_properties,
            indexed_properties,
        },
        operations: candidate
            .operations
            .iter()
            .map(|op| ExecutePlanTypedOp {
                params_blob: op.params_blob.clone(),
                seed: op.seed.clone(),
            })
            .collect(),
        batch_mode,
    };
    args.validate()?;
    Ok(args)
}

/// Validate that a Graph-ingress typed batch envelope is eligible for the V1 path.
///
/// This is the Graph-side mirror of `classify_typed_batch_eligibility`: it re-checks the
/// structural constraints that the Graph can verify without trusting the Router. It returns
/// `Ok(())` only when the plan and every operation satisfy the typed V1 contract.
pub fn validate_typed_batch_eligibility_for_graph(
    args: &ExecutePlanBatchTypedArgs,
) -> Result<(), &'static str> {
    if args.operations.is_empty() {
        return Err("typed V1 requires at least one operation");
    }
    let plan_bounds = validate_typed_batch_plan(&args.shared.plan_blob)?;
    for op in &args.operations {
        if !op.seed.entries.is_empty() {
            return Err("typed V1 requires empty grouped seed entries");
        }
        if !op.seed.complete_prefix_rows {
            return Err("typed V1 requires complete_prefix_rows=true");
        }
        if op.seed.rows.len() > 1024 {
            return Err("typed V1 supports at most 1024 seed rows per operation");
        }
        if op.params_blob.len() > gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        {
            return Err("typed V1 params_blob exceeds the safe payload limit");
        }
    }
    validate_typed_batch_response_bound(
        args.operations.iter().map(|op| op.seed.rows.len()),
        plan_bounds,
    )?;
    Ok(())
}

fn is_empty_optional_slice<T>(v: Option<&Vec<T>>) -> bool {
    v.is_none_or(|x| x.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Encode;
    use gleaph_gql::ast::Expr;
    use gleaph_gql_planner::plan::ProjectColumn;
    use gleaph_gql_planner::plan::{PhysicalPlan, PlanOp};
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_graph_kernel::entry::ConstraintNameId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::{
        ExecutePlanArgs, SeedBindingEntry, SeedBindingsWire, SeedRowWire, UniqueClaimDispatch,
    };
    use std::rc::Rc;

    fn posted_plan(requires_write_path: bool) -> (Vec<u8>, PhysicalPlan) {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("u"),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::InsertVertex {
                variable: Some(Rc::from("p")),
                labels: vec!["Post".into()],
                properties: vec![],
            },
            PlanOp::InsertEdge {
                variable: None,
                src: Rc::from("u"),
                dst: Rc::from("p"),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                labels: vec!["POSTED".into()],
                properties: vec![],
            },
        ]);
        let blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
            .expect("encode plan");
        (blob, plan)
    }

    fn query_plan() -> Vec<u8> {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::var("n"),
                    alias: Some(Rc::from("n")),
                }],
                distinct: false,
            },
        ]);
        encode_block_plans(std::slice::from_ref(&plan), false).expect("encode plan")
    }

    fn write_plan_with_return() -> Vec<u8> {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("u"),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::InsertVertex {
                variable: Some(Rc::from("p")),
                labels: vec!["Post".into()],
                properties: vec![],
            },
            PlanOp::InsertEdge {
                variable: None,
                src: Rc::from("u"),
                dst: Rc::from("p"),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                labels: vec!["POSTED".into()],
                properties: vec![],
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::var("p"),
                    alias: Some(Rc::from("p")),
                }],
                distinct: false,
            },
        ]);
        encode_block_plans(std::slice::from_ref(&plan), true).expect("encode plan")
    }

    fn base_op() -> ExecutePlanArgs {
        let (plan_blob, _) = posted_plan(true);
        ExecutePlanArgs {
            target_shard_id: ShardId(1),
            element_id_encoding_key: [0u8; 16],
            mutation_id: Some(7),
            plan_blob,
            params_blob: vec![4, 5],
            mode: GqlExecutionMode::Update,
            seed_bindings_blob: Some(
                Encode!(&SeedBindingsWire {
                    entries: vec![],
                    rows: vec![],
                    complete_prefix_rows: true,
                })
                .expect("encode"),
            ),
            resolved_labels: None,
            resolved_properties: None,
            indexed_properties: None,
            unique_claims: None,
            constrained_properties: None,
            local_unique_claims: None,
            local_constrained_properties: None,
            indexed_embeddings: None,
            resolved_search_blob: None,
        }
    }

    fn with_seed_blob(op: &mut ExecutePlanArgs, rows: Vec<SeedRowWire>) {
        op.seed_bindings_blob = Some(
            Encode!(&SeedBindingsWire {
                entries: vec![],
                rows,
                complete_prefix_rows: true,
            })
            .expect("encode"),
        );
    }

    #[test]
    fn empty_list_is_ineligible() {
        let result = classify_typed_batch_eligibility(&[]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("empty"))
        );
    }

    #[test]
    fn query_mode_is_ineligible() {
        let mut op = base_op();
        op.mode = GqlExecutionMode::Query;
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("update-only"))
        );
    }

    #[test]
    fn mixed_shard_is_ineligible() {
        let op1 = base_op();
        let mut op2 = base_op();
        op2.target_shard_id = ShardId(2);
        let result = classify_typed_batch_eligibility(&[op1, op2]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("single target shard"))
        );
    }

    #[test]
    fn grouped_entries_are_ineligible() {
        let mut op = base_op();
        op.seed_bindings_blob = Some(
            Encode!(&SeedBindingsWire {
                entries: vec![SeedBindingEntry {
                    variable: "x".into(),
                    local_vertex_ids: vec![1],
                    local_edge_postings: vec![],
                }],
                rows: vec![],
                complete_prefix_rows: true,
            })
            .expect("encode"),
        );
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("grouped"))
        );
    }

    #[test]
    fn incomplete_prefix_rows_are_ineligible() {
        let mut op = base_op();
        op.seed_bindings_blob = Some(
            Encode!(&SeedBindingsWire {
                entries: vec![],
                rows: vec![],
                complete_prefix_rows: false,
            })
            .expect("encode"),
        );
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("complete_prefix_rows"))
        );
    }

    #[test]
    fn uniqueness_claims_are_ineligible() {
        let mut op = base_op();
        op.unique_claims = Some(vec![UniqueClaimDispatch {
            claim_ordinal: 0,
            constraint_id: ConstraintNameId::from_raw(1),
            encoded_value: vec![1],
        }]);
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("uniqueness"))
        );
    }

    #[test]
    fn resolved_search_is_ineligible() {
        let mut op = base_op();
        op.resolved_search_blob = Some(vec![1, 2, 3]);
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("resolved search"))
        );
    }

    #[test]
    fn homogeneous_complete_row_group_is_eligible() {
        let mut op1 = base_op();
        let mut op2 = base_op();
        with_seed_blob(
            &mut op1,
            vec![SeedRowWire {
                vertex_bindings: vec![],
                float64_bindings: vec![],
            }],
        );
        with_seed_blob(
            &mut op2,
            vec![SeedRowWire {
                vertex_bindings: vec![],
                float64_bindings: vec![],
            }],
        );
        let result = classify_typed_batch_eligibility(&[op1.clone(), op2.clone()]);
        let TypedBatchEligibility::Eligible(candidate) = result else {
            panic!("expected eligible, got {result:?}");
        };
        assert_eq!(candidate.target_shard_id, ShardId(1));
        assert_eq!(candidate.mutation_id, 7);
        assert_eq!(candidate.operations.len(), 2);
        assert!(candidate.operations[0].seed.complete_prefix_rows);
    }

    #[test]
    fn mismatched_plan_blob_is_ineligible() {
        let (plan_blob_a, _) = posted_plan(true);
        let plan_b = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("u"),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::InsertVertex {
                variable: Some(Rc::from("p")),
                labels: vec!["Comment".into()],
                properties: vec![],
            },
            PlanOp::InsertEdge {
                variable: None,
                src: Rc::from("u"),
                dst: Rc::from("p"),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                labels: vec!["POSTED".into()],
                properties: vec![],
            },
        ]);
        let plan_blob_b =
            encode_block_plans(std::slice::from_ref(&plan_b), true).expect("encode plan");
        assert_ne!(plan_blob_a, plan_blob_b);

        let mut op1 = base_op();
        op1.plan_blob = plan_blob_a;
        let mut op2 = base_op();
        op2.plan_blob = plan_blob_b;
        let result = classify_typed_batch_eligibility(&[op1, op2]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("shared plan_blob"))
        );
    }

    #[test]
    fn non_write_path_plan_is_ineligible() {
        let (write_plan_blob, _) = posted_plan(true);
        let (read_plan_blob, _) = posted_plan(false);
        assert_ne!(write_plan_blob, read_plan_blob);

        let mut op = base_op();
        op.plan_blob = read_plan_blob;
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("write-path"))
        );
    }

    #[test]
    fn query_only_plan_is_ineligible() {
        let mut op = base_op();
        op.plan_blob = query_plan();
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("write-path"))
        );
    }

    #[test]
    fn returned_columns_remain_eligible_on_update_transport() {
        let mut op = base_op();
        op.plan_blob = write_plan_with_return();
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(matches!(result, TypedBatchEligibility::Eligible(_)));
    }

    #[test]
    fn row_expanding_for_plan_is_ineligible() {
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("u"),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::For {
                variable: Rc::from("item"),
                list: Expr::new(gleaph_gql::ast::ExprKind::ListLiteral(vec![
                    Expr::int(1),
                    Expr::int(2),
                ])),
                ordinality: None,
                offset_keyword: false,
            },
            PlanOp::InsertVertex {
                variable: Some(Rc::from("p")),
                labels: vec!["Post".into()],
                properties: vec![],
            },
        ]);
        let blob = encode_block_plans(std::slice::from_ref(&plan), true).expect("encode plan");
        let mut op = base_op();
        op.plan_blob = blob;
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("row-cardinality-changing"))
        );
    }

    #[test]
    fn non_single_anchor_threaded_bundle_is_ineligible() {
        // Two existing-state reads after the anchor break the single-anchor-threaded contract.
        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("u"),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::NodeScan {
                variable: Rc::from("x"),
                label: Some("Group".into()),
                property_projection: None,
            },
            PlanOp::InsertEdge {
                variable: None,
                src: Rc::from("u"),
                dst: Rc::from("x"),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                labels: vec!["MEMBER".into()],
                properties: vec![],
            },
        ]);
        let blob = encode_block_plans(std::slice::from_ref(&plan), true).expect("encode plan");
        let mut op = base_op();
        op.plan_blob = blob;
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(
            matches!(result, TypedBatchEligibility::Ineligible { reason } if reason.contains("single-anchor threaded bundle"))
        );
    }

    #[test]
    fn empty_seed_rows_are_eligible() {
        let op = base_op();
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(matches!(result, TypedBatchEligibility::Eligible(_)));
    }

    #[test]
    fn validate_typed_batch_plan_accepts_posted_plan() {
        let (blob, _) = posted_plan(true);
        let bounds = validate_typed_batch_plan(&blob).expect("posted plan should validate");
        assert_eq!(bounds.insert_edges_per_input_row, 1);
    }

    #[test]
    fn validate_typed_batch_plan_rejects_malformed_blob() {
        let err = validate_typed_batch_plan(&[1, 2, 3]).expect_err("invalid blob");
        assert!(err.contains("decodable"));
    }

    #[test]
    fn validate_typed_batch_eligibility_for_graph_rejects_empty_operations() {
        let args = ExecutePlanBatchTypedArgs {
            shared: gleaph_graph_kernel::plan_exec::ExecutePlanBatchTypedShared {
                target_shard_id: ShardId(1),
                element_id_encoding_key: [0u8; 16],
                mutation_id: 1,
                plan_blob: posted_plan(true).0,
                resolved_labels: None,
                resolved_properties: None,
                indexed_properties: None,
            },
            operations: vec![],
            batch_mode: gleaph_graph_kernel::plan_exec::ExecutePlanBatchMode::Dynamic,
        };
        let err = validate_typed_batch_eligibility_for_graph(&args).expect_err("empty ops");
        assert!(err.contains("at least one operation"));
    }

    #[test]
    fn production_builder_rejects_empty_and_full_request_oversize_candidates() {
        let TypedBatchEligibility::Eligible(mut candidate) =
            classify_typed_batch_eligibility(&[base_op()])
        else {
            panic!("base operation must be typed-eligible");
        };
        candidate.operations.clear();
        assert!(
            build_and_validate_typed_batch_args(
                &candidate,
                None,
                None,
                None,
                &IndexedEmbeddingCatalog::default(),
                ExecutePlanBatchMode::Dynamic,
            )
            .expect_err("empty candidate")
            .contains("1..=1024")
        );

        let TypedBatchEligibility::Eligible(mut candidate) =
            classify_typed_batch_eligibility(&[base_op()])
        else {
            panic!("base operation must be typed-eligible");
        };
        let mut operation = candidate.operations[0].clone();
        operation.params_blob = vec![0; 3_000];
        candidate.operations = vec![operation; 1024];
        assert!(
            build_and_validate_typed_batch_args(
                &candidate,
                None,
                None,
                None,
                &IndexedEmbeddingCatalog::default(),
                ExecutePlanBatchMode::Dynamic,
            )
            .expect_err("full request must exceed portable payload bound")
            .contains("request exceeds")
        );

        let indexed_embeddings = IndexedEmbeddingCatalog {
            embeddings: vec![gleaph_graph_kernel::vector_index::IndexedEmbeddingSpec {
                embedding_name_id: 5,
                index_id: 11,
                kind: gleaph_graph_kernel::vector_index::VectorIndexKind::IvfFlat,
                metric: gleaph_graph_kernel::vector_index::VectorMetric::L2Squared,
                encoding: gleaph_graph_kernel::vector_index::VectorEncoding::F32,
                dims: 16,
            }],
        };
        assert!(
            build_and_validate_typed_batch_args(
                &candidate,
                None,
                None,
                None,
                &indexed_embeddings,
                ExecutePlanBatchMode::Dynamic,
            )
            .expect_err("indexed embeddings require scalar dispatch")
            .contains("indexed-embedding")
        );
    }

    #[test]
    fn response_bound_accepts_measured_shape_and_rejects_unreturnable_shape() {
        let bounds = TypedBatchPlanBounds {
            insert_edges_per_input_row: 1,
        };
        validate_typed_batch_response_bound(std::iter::repeat_n(1, 512), bounds)
            .expect("measured POSTED shape fits");

        let oversized = TypedBatchPlanBounds {
            insert_edges_per_input_row: MAX_TYPED_BATCH_INSERT_EDGE_OPS,
        };
        let err = validate_typed_batch_response_bound(std::iter::repeat_n(1024, 1024), oversized)
            .expect_err("worst-case response must be rejected before allocation");
        assert!(err.contains("worst-case response exceeds"));
    }
}
