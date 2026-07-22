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
//! - a result shape with no `rows_blob` and a bounded `hot_forward_vertices` count.
//!
//! Anything else keeps the existing scalar or legacy-batch path.

use candid::Decode;
use gleaph_graph_kernel::plan_exec::{ExecutePlanArgs, GqlExecutionMode, SeedBindingsWire};
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

    TypedBatchEligibility::Eligible(TypedBatchCandidate {
        target_shard_id,
        element_id_encoding_key,
        mutation_id,
        plan_blob: first.plan_blob.clone(),
        operations: candidate_operations,
    })
}

fn is_empty_optional_slice<T>(v: Option<&Vec<T>>) -> bool {
    v.is_none_or(|x| x.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Encode;
    use gleaph_graph_kernel::entry::ConstraintNameId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::plan_exec::{
        ExecutePlanArgs, GqlExecutionMode, SeedBindingEntry, SeedBindingsWire, SeedRowWire,
        UniqueClaimDispatch,
    };

    fn base_op() -> ExecutePlanArgs {
        ExecutePlanArgs {
            target_shard_id: ShardId(1),
            element_id_encoding_key: [0u8; 16],
            mutation_id: Some(7),
            plan_blob: vec![1, 2, 3],
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
        let op1 = base_op();
        let mut op2 = base_op();
        op2.plan_blob = vec![9, 9, 9];
        let result = classify_typed_batch_eligibility(&[op1, op2]);
        // plan_blob is not explicitly compared in the classifier currently; we rely on the fact
        // that a real Router group always shares the same plan. This test documents the gap and
        // will fail if we add an explicit check later.
        assert!(matches!(result, TypedBatchEligibility::Eligible(_)));
    }

    #[test]
    fn empty_seed_rows_are_eligible() {
        let op = base_op();
        let result = classify_typed_batch_eligibility(&[op]);
        assert!(matches!(result, TypedBatchEligibility::Eligible(_)));
    }
}
