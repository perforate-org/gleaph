//! Batch ingress data types and budget constants for Router GQL mutations.
//!
//! Mutations are prepared in [`crate::gql::prepare_mutation_for_batch`] and, if they require
//! Graph execution, handed off as [`PreparedMutation`] values. The current ingress path executes
//! prepared mutations sequentially; cross-mutation coalescing can be layered here later.

use std::collections::BTreeMap;

use candid::Principal;

/// Headroom reserved for the Router to finish an ingress after preparing mutations.
/// Covers prefetch/dispatch, response encode, and any final bookkeeping.
pub(crate) const ROUTER_WORK_HEADROOM: u64 = 4_000_000_000;

/// One mutation prepared through the classification phase.
///
/// All async prefetch (journal, anchors) is already resolved; the executor only needs to run
/// the Graph dispatch and post-processing phases.
#[derive(Clone)]
pub(crate) struct PreparedMutation {
    /// Whether the program contains DML.
    pub has_dml: bool,
    /// Merge mode for federated results.
    pub merge_mode: crate::federation::FederatedMergeMode,
    /// Shards this mutation was dispatched to.
    pub dispatches: Vec<crate::federation::ShardDispatch>,
    /// Mutation id if this mutation reserved one.
    pub mutation_id: Option<gleaph_graph_kernel::plan_exec::MutationId>,
    /// Unique claims to confirm after dispatch.
    pub unique_claims: Option<Vec<gleaph_graph_kernel::plan_exec::UniqueClaimDispatch>>,
    /// Graph canisters carrying unique proofs.
    pub unique_proof_targets: Vec<Principal>,
    /// Constrained properties to release after dispatch.
    pub constrained_properties:
        Option<Vec<gleaph_graph_kernel::plan_exec::ConstrainedPropertyDispatch>>,
    /// Graph canisters carrying release effects.
    pub unique_release_targets: Vec<Principal>,
    /// Local unique claims for ShardLocalGlobal fast path.
    pub local_unique_claims: Option<Vec<gleaph_graph_kernel::plan_exec::UniqueClaimDispatch>>,
    /// Local constrained properties.
    pub local_constrained_properties:
        Option<Vec<gleaph_graph_kernel::plan_exec::ConstrainedPropertyDispatch>>,
    /// Indexed property catalog supplied to Graph.
    pub indexed_properties: gleaph_graph_kernel::index::IndexedPropertyCatalog,
    /// Indexed embedding catalog supplied to Graph.
    pub indexed_embeddings: gleaph_graph_kernel::vector_index::IndexedEmbeddingCatalog,
    /// Element id encoding key for this graph.
    pub element_id_encoding_key: gleaph_graph_kernel::federation::ElementIdEncodingKey,
    /// Resolved labels.
    pub resolved_labels: gleaph_graph_kernel::plan_exec::ResolvedLabelTable,
    /// Resolved properties.
    pub resolved_properties: gleaph_graph_kernel::plan_exec::ResolvedPropertyTable,
    /// Encoded plan blob ready for Graph.
    pub plan_blob: Vec<u8>,
    /// Decoded parameter map.
    pub pmap: BTreeMap<String, gleaph_gql::Value>,
    /// Raw params blob.
    pub params: Vec<u8>,
    /// Execution mode.
    pub mode: gleaph_graph_kernel::plan_exec::GqlExecutionMode,
    /// Physical plans (needed for post-dispatch hot-vertex finalization).
    pub plans: Vec<gleaph_gql_planner::PhysicalPlan>,
}

/// One input mutation inside a bulk group that shares a single plan and mutation id.
#[derive(Clone)]
pub(crate) struct BulkGroupItem {
    /// Encoded parameter blob for this item.
    pub params: Vec<u8>,
    /// Decoded parameter map for this item.
    pub pmap: BTreeMap<String, gleaph_gql::Value>,
}

/// A group of input mutations that share the same query plan and are dispatched under one
/// `mutation_id` to each target Graph canister (ADR 0044).
#[derive(Clone)]
pub(crate) struct PreparedBulkGroup {
    /// Common prepared state, taken from the first item.
    pub base: PreparedMutation,
    /// Additional items beyond the first. The first item is represented by `base.params` /
    /// `base.pmap` and is not duplicated here.
    pub extra_items: Vec<BulkGroupItem>,
}
