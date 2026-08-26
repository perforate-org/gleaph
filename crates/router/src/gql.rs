//! Router-side GQL parse, plan, index seed routing, and graph dispatch.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use candid::{Encode, Principal};
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::GqlWireRows;
use gleaph_gql_ic::decode_gql_params_blob;
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_gql_planner::{PhysicalPlan, PlanOp};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ClaimId, EffectId, ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::{
    IndexIntersectionRequest, IndexIntersectionResult, IndexedPropertyCatalog, PhysicalIndexId,
    PostingHit, PostingRangeRequest, ValuePostingCount,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanResult, GetMutationJournalEntriesArgs, GqlExecutionMode, GqlQueryResult,
    GraphMutationJournalEntryWire, GraphMutationRequestIdentityV1, GraphMutationRetirementWireV1,
    GraphOrderedEdgeBatchReceiptV1, GraphOrderedEdgeBatchResult, GraphOrderedEdgeBatchResultV1,
    GraphOrderedMixedBatchReceiptV1, GraphOrderedMixedBatchResult, GraphOrderedMixedBatchResultV1,
    GraphOrderedVertexBatchReceiptV1, GraphOrderedVertexBatchResult,
    GraphOrderedVertexBatchResultV1, MutationId, MutationJournalState, MutationToken,
    MutationTokenShard, ReadMode, ResolvedLabelTable, ResolvedPropertyTable, ResolvedSearchWire,
    ShardEventSeq, UniqueClaimDispatch,
};
use gleaph_graph_kernel::vector_index::IndexedEmbeddingCatalog;
use gleaph_message_sizing::{FitError, SizeHint, SizingPolicy, adaptive_fitting_prefix};
use ic_cdk::api::msg_caller;

#[cfg(feature = "batch-instr-log")]
fn log_router_dispatch_phase(phase: &str, cost: u64) {
    crate::instr_log::push(format!(
        "GLEAPH_ROUTER_DISPATCH phase={} cost={}",
        phase, cost
    ));
}

#[cfg(not(feature = "batch-instr-log"))]
#[allow(dead_code)]
#[inline]
fn log_router_dispatch_phase(_phase: &str, _cost: u64) {}

#[cfg(feature = "batch-instr-log")]
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SeedResolutionMetrics {
    cache_lookup_clone: u64,
    remote_index_await: u64,
    anchor_intersection: u64,
    cache_hits: u64,
    cache_misses: u64,
}

#[cfg(not(feature = "batch-instr-log"))]
#[derive(Default, Debug, Clone, Copy)]
pub(crate) struct SeedResolutionMetrics;

#[cfg(feature = "batch-instr-log")]
fn log_router_preflight(kind: &str, cost: u64) {
    crate::instr_log::push(format!(
        "GLEAPH_ROUTER_PREFLIGHT kind={} cost={}",
        kind, cost
    ));
}

#[cfg(not(feature = "batch-instr-log"))]
#[allow(dead_code)]
#[inline]
fn log_router_preflight(_kind: &str, _cost: u64) {}

#[cfg(feature = "batch-instr-log")]
struct PrepareInstrLogger {
    start: u64,
    last: u64,
    key: String,
    checkpoints: Vec<(&'static str, u64)>,
}

#[cfg(feature = "batch-instr-log")]
impl PrepareInstrLogger {
    fn new(key: Option<&str>) -> Self {
        let now = crate::current_instruction_counter();
        Self {
            start: now,
            last: now,
            key: key.unwrap_or("-").to_string(),
            checkpoints: Vec::new(),
        }
    }
    fn mark(&mut self, phase: &'static str) {
        let now = crate::current_instruction_counter();
        let cost = now.saturating_sub(self.last);
        self.checkpoints.push((phase, cost));
        self.last = now;
    }
}

#[cfg(feature = "batch-instr-log")]
impl Drop for PrepareInstrLogger {
    fn drop(&mut self) {
        let total = crate::current_instruction_counter().saturating_sub(self.start);
        let mut parts = String::new();
        for (phase, cost) in &self.checkpoints {
            if !parts.is_empty() {
                parts.push(' ');
            }
            parts.push_str(&format!("{}={}", phase, cost));
        }
        crate::instr_log::push(format!(
            "GLEAPH_ROUTER_PREPARE key={} total={} {}",
            self.key, total, parts
        ));
    }
}

#[cfg(not(feature = "batch-instr-log"))]
struct PrepareInstrLogger;

#[cfg(not(feature = "batch-instr-log"))]
impl PrepareInstrLogger {
    #[inline]
    fn new(_key: Option<&str>) -> Self {
        Self
    }
    #[inline]
    fn mark(&mut self, _phase: &'static str) {}
}

use nohash_hasher::IntSet;
use std::collections::{HashMap, HashSet};

use crate::execution_path::check_adhoc_execution_path;
use crate::facade::stable::label_stats::RouterMutationShardV1;
use crate::facade::stable::label_stats::{
    ClientMutationKey, OrderedEdgeBatchTargetProgressV1, OrderedMixedBatchTargetProgressV1,
    OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1, RouterMutationRecord,
    RouterMutationRequestIdentityV1, RouterOrderedEdgeBatchTargetV1,
    RouterOrderedMixedBatchTargetV1, RouterOrderedVertexBatchTargetV1,
};
use crate::facade::stable::reservation_catalog::ConfirmOutcome;
use crate::facade::store::uniqueness::{
    ConstrainedDispatchSplit, LocalUniqueClaim, plan_can_release,
};
use crate::facade::store::{ClientMutationReservation, RouterStore};
use crate::federation::{
    AggregateIndexFastPath, FederatedMergeMode, SeedHits, ShardDispatch, ShardingPolicy,
    apply_federated_aggregate_having, collect_label_hits_for_shards,
    collect_label_intersection_hits_for_shards, empty_execute_plan_result,
    federated_dispatch_plan_blob, federated_merge_mode_from_plans,
    gql_query_result_from_label_live_count, gql_query_result_from_posting_counts,
    group_dispatches_by_graph, merge_execute_plan_result, packed_vertices_exceed_fast_path_budget,
    posting_hits_exceed_fast_path_budget, routings_to_dispatches, sharding_policy_for,
    split_label_and_property_anchors, try_aggregate_index_fast_path,
    try_label_count_telemetry_fast_path, vertex_label_live_count,
};

use crate::graph_client::{
    ack_label_stats_deltas_through, ack_unique_effects, execute_ordered_edge_batch_on_graph,
    execute_ordered_mixed_batch_on_graph, execute_ordered_vertex_batch_on_graph,
    execute_plan_batch_on_graph, execute_plan_on_graph, get_mutation_journal_entries,
    get_mutation_journal_entry, index_pending_min_mutation_id, list_pending_label_stats_deltas,
    read_unique_effect_proof, read_unique_release_effects, retire_ordered_mixed_mutation_on_graph,
    retire_ordered_mutation_on_graph, retire_ordered_vertex_mutation_on_graph,
};
use crate::index_catalog::graph_stats_for;
use crate::index_lookup::{IndexLookup, RouterIndexLookup};
use crate::planner_stats::RouterGraphStats;
use crate::rbac::{authorize_adhoc_gql, authorize_index_ddl};
use crate::seed::{IndexAnchor, SeedAnchorSet, SeedProbe};
use crate::state::RouterError;

fn mutation_key_for(caller: Principal, graph_id: GraphId, client_key: &str) -> ClientMutationKey {
    ClientMutationKey::new(caller, graph_id, client_key.to_owned())
}

pub(crate) type BatchDispatchResult = (ShardDispatch, Option<Result<ExecutePlanResult, String>>);

pub(crate) const BATCH_DEFERRED_ERROR: &str = "batch operation deferred by instruction budget";

#[cfg(test)]
struct OrderedReconciliationTestDispatches {
    edge: Option<Result<GraphOrderedEdgeBatchResult, String>>,
    vertex: Option<Result<GraphOrderedVertexBatchResult, String>>,
    mixed: Option<Result<GraphOrderedMixedBatchResult, String>>,
    edge_args: Option<gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs>,
    vertex_args: Option<gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs>,
    mixed_args: Option<gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs>,
    calls: u32,
}

#[cfg(test)]
impl OrderedReconciliationTestDispatches {
    fn new() -> Self {
        Self {
            edge: None,
            vertex: None,
            mixed: None,
            edge_args: None,
            vertex_args: None,
            mixed_args: None,
            calls: 0,
        }
    }
}

#[cfg(test)]
thread_local! {
    static ORDERED_RECONCILIATION_TEST_DISPATCHES:
        RefCell<OrderedReconciliationTestDispatches> =
        RefCell::new(OrderedReconciliationTestDispatches::new());
    static ORDERED_TEST_CALLER: RefCell<Option<Principal>> = const { RefCell::new(None) };
}

#[cfg(test)]
fn ordered_insert_caller() -> Principal {
    ORDERED_TEST_CALLER.with(|caller| caller.borrow().unwrap_or_else(msg_caller))
}

#[cfg(not(test))]
fn ordered_insert_caller() -> Principal {
    msg_caller()
}

#[cfg(test)]
fn set_ordered_test_caller(caller: Option<Principal>) {
    ORDERED_TEST_CALLER.with(|current| *current.borrow_mut() = caller);
}

#[cfg(test)]
fn reset_test_reconciliation_dispatches() {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| {
        *state.borrow_mut() = OrderedReconciliationTestDispatches::new();
    });
}

#[cfg(test)]
fn set_test_edge_dispatch(result: Result<GraphOrderedEdgeBatchResult, String>) {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow_mut().edge = Some(result));
}

#[cfg(test)]
fn set_test_vertex_dispatch(result: Result<GraphOrderedVertexBatchResult, String>) {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow_mut().vertex = Some(result));
}

#[cfg(test)]
fn set_test_mixed_dispatch(result: Result<GraphOrderedMixedBatchResult, String>) {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow_mut().mixed = Some(result));
}

#[cfg(test)]
fn test_reconciliation_dispatch_calls() -> u32 {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow().calls)
}

#[cfg(test)]
fn test_edge_dispatch_args() -> Option<gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs> {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow().edge_args.clone())
}

#[cfg(test)]
fn test_vertex_dispatch_args() -> Option<gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs>
{
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow().vertex_args.clone())
}

#[cfg(test)]
fn test_mixed_dispatch_args() -> Option<gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs>
{
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| state.borrow().mixed_args.clone())
}

#[cfg(test)]
fn take_test_edge_dispatch(
    args: gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs,
) -> Option<Result<GraphOrderedEdgeBatchResult, String>> {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| {
        let mut state = state.borrow_mut();
        let result = state.edge.take();
        if result.is_some() {
            state.calls += 1;
            state.edge_args = Some(args);
        }
        result
    })
}

#[cfg(test)]
fn take_test_vertex_dispatch(
    args: gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs,
) -> Option<Result<GraphOrderedVertexBatchResult, String>> {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| {
        let mut state = state.borrow_mut();
        let result = state.vertex.take();
        if result.is_some() {
            state.calls += 1;
            state.vertex_args = Some(args);
        }
        result
    })
}

#[cfg(test)]
fn take_test_mixed_dispatch(
    args: gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs,
) -> Option<Result<GraphOrderedMixedBatchResult, String>> {
    ORDERED_RECONCILIATION_TEST_DISPATCHES.with(|state| {
        let mut state = state.borrow_mut();
        let result = state.mixed.take();
        if result.is_some() {
            state.calls += 1;
            state.mixed_args = Some(args);
        }
        result
    })
}

/// Cached plan/plan-blob for a concrete `(caller, graph, query)` shape.
#[derive(Clone)]
pub(crate) struct CachedPlanDispatch {
    /// The graph on which the cached plan executes.
    dispatch_graph_id: GraphId,
    /// The Router dispatch decision (EffectiveGraph or Single only for now).
    dispatch: crate::use_graph::UseGraphV2Dispatch,
    /// The encoded plan blob ready to send to a Graph shard.
    plan_blob: Vec<u8>,
}

/// Preflight context shared across one internal GQL plan-batch ingress.
///
/// Holds coalesced inter-canister lookup results so that N mutations referencing the same
/// seed anchor or shard do not issue N identical Router→Graph/Index calls during planning.
/// It also caches parsed/planned/encoded query shapes so repeated query strings inside the
/// same ingress avoid redundant parse, validate, plan, and plan-blob encode work.
/// Read-only graph-derived catalog state cached for the lifetime of an ingress.
#[derive(Clone)]
pub(crate) struct GraphCatalogSnapshot {
    element_id_encoding_key: [u8; 16],
    indexed_properties: IndexedPropertyCatalog,
    indexed_embeddings: IndexedEmbeddingCatalog,
}

#[derive(Clone)]
pub(crate) struct PreflightContext {
    /// `(anchor, shard_id)` -> resolved seed hits.
    pub(crate) anchor_hits:
        Rc<RefCell<std::collections::HashMap<(IndexAnchor, ShardId), SeedHits>>>,
    /// `(graph_canister, mutation_id)` -> cached journal entry.
    pub(crate) journal_entries:
        Rc<RefCell<HashMap<(Principal, MutationId), Option<GraphMutationJournalEntryWire>>>>,
    /// `graph_canister` -> cached `index_pending_min_mutation_id` result.
    pub(crate) pending_min_mutation_id: Rc<RefCell<HashMap<Principal, Option<MutationId>>>>,
    /// `(caller, graph_id, query)` -> cached dispatch + plan blob.
    pub(crate) plan_cache: Rc<RefCell<HashMap<(Principal, GraphId, String), CachedPlanDispatch>>>,
    /// `(graph_id, plan_blob)` -> resolved labels from the planner.
    resolved_labels: Rc<RefCell<HashMap<(GraphId, Vec<u8>), ResolvedLabelTable>>>,
    /// `(graph_id, plan_blob)` -> resolved properties from the planner.
    resolved_properties: Rc<RefCell<HashMap<(GraphId, Vec<u8>), ResolvedPropertyTable>>>,
    /// `graph_id` -> read-only derived catalog snapshot for dispatch.
    graph_catalog: Rc<RefCell<HashMap<GraphId, GraphCatalogSnapshot>>>,
}

impl PreflightContext {
    /// Return a cached dispatch and plan blob for the given query shape.
    pub(crate) fn get_cached_plan(
        &self,
        caller: Principal,
        graph_id: GraphId,
        query: &str,
    ) -> Option<CachedPlanDispatch> {
        self.plan_cache
            .borrow()
            .get(&(caller, graph_id, query.to_string()))
            .cloned()
    }

    /// Cache a parsed/planned/encoded query shape for the lifetime of this ingress.
    pub(crate) fn insert_cached_plan(
        &self,
        caller: Principal,
        graph_id: GraphId,
        query: String,
        dispatch_graph_id: GraphId,
        dispatch: crate::use_graph::UseGraphV2Dispatch,
        plan_blob: Vec<u8>,
    ) {
        self.plan_cache.borrow_mut().insert(
            (caller, graph_id, query),
            CachedPlanDispatch {
                dispatch_graph_id,
                dispatch,
                plan_blob,
            },
        );
    }

    /// Return cached resolved labels for this plan shape, if present.
    pub(crate) fn get_resolved_labels(
        &self,
        graph_id: GraphId,
        plan_blob: &[u8],
    ) -> Option<ResolvedLabelTable> {
        self.resolved_labels
            .borrow()
            .get(&(graph_id, plan_blob.to_vec()))
            .cloned()
    }

    /// Cache resolved labels for this plan shape.
    pub(crate) fn insert_resolved_labels(
        &self,
        graph_id: GraphId,
        plan_blob: &[u8],
        labels: ResolvedLabelTable,
    ) {
        self.resolved_labels
            .borrow_mut()
            .insert((graph_id, plan_blob.to_vec()), labels);
    }

    /// Return cached resolved properties for this plan shape, if present.
    pub(crate) fn get_resolved_properties(
        &self,
        graph_id: GraphId,
        plan_blob: &[u8],
    ) -> Option<ResolvedPropertyTable> {
        self.resolved_properties
            .borrow()
            .get(&(graph_id, plan_blob.to_vec()))
            .cloned()
    }

    /// Cache resolved properties for this plan shape.
    pub(crate) fn insert_resolved_properties(
        &self,
        graph_id: GraphId,
        plan_blob: &[u8],
        properties: ResolvedPropertyTable,
    ) {
        self.resolved_properties
            .borrow_mut()
            .insert((graph_id, plan_blob.to_vec()), properties);
    }

    /// Return cached graph catalog snapshot, if present.
    pub(crate) fn get_graph_catalog(&self, graph_id: GraphId) -> Option<GraphCatalogSnapshot> {
        self.graph_catalog.borrow().get(&graph_id).cloned()
    }

    /// Cache graph catalog snapshot for this graph.
    pub(crate) fn insert_graph_catalog(
        &self,
        graph_id: GraphId,
        element_id_encoding_key: [u8; 16],
        indexed_properties: IndexedPropertyCatalog,
        indexed_embeddings: IndexedEmbeddingCatalog,
    ) {
        self.graph_catalog.borrow_mut().insert(
            graph_id,
            GraphCatalogSnapshot {
                element_id_encoding_key,
                indexed_properties,
                indexed_embeddings,
            },
        );
    }

    async fn resolve_anchor_hits<I: IndexLookup + ?Sized>(
        &self,
        index: &I,
        anchor: &IndexAnchor,
        shard_id: ShardId,
        metrics: &mut SeedResolutionMetrics,
    ) -> Result<SeedHits, String> {
        let key = (anchor.clone(), shard_id);
        {
            let cache = self.anchor_hits.borrow();
            if let Some(hits) = cache.get(&key) {
                #[cfg(feature = "batch-instr-log")]
                {
                    let clone_start = crate::current_instruction_counter();
                    let cloned = hits.clone();
                    metrics.cache_lookup_clone +=
                        crate::current_instruction_counter().saturating_sub(clone_start);
                    metrics.cache_hits += 1;
                    return Ok(cloned);
                }
                #[cfg(not(feature = "batch-instr-log"))]
                {
                    return Ok(hits.clone());
                }
            }
        }
        #[cfg(feature = "batch-instr-log")]
        {
            metrics.cache_misses += 1;
        }

        let hits = lookup_anchor_hits(index, anchor, &[shard_id], metrics).await?;

        self.anchor_hits.borrow_mut().insert(key, hits.clone());
        Ok(hits)
    }

    /// Read mutation journal entries for the given (graph_canister, mutation_id) pairs,
    /// issuing at most one Graph canister call per target canister and per Candid payload limit.
    /// Results are cached and returned in the same order as the input pairs. The Graph canister
    /// may stop early when it nears its instruction budget; this helper pages forward transparently.
    async fn resolve_journal_entries(
        &self,
        targets: &[(Principal, MutationId)],
    ) -> Result<Vec<Option<GraphMutationJournalEntryWire>>, RouterError> {
        self.resolve_journal_entries_impl(targets, false).await
    }

    /// Eagerly prefetch mutation journal entries for existing mutations before dispatch. Like
    /// `resolve_journal_entries`, but records a cache miss as `None` instead of returning an error.
    async fn prefetch_journal_entries(&self, targets: &[(Principal, MutationId)]) {
        let _ = self.resolve_journal_entries_impl(targets, true).await;
    }

    async fn resolve_journal_entries_impl(
        &self,
        targets: &[(Principal, MutationId)],
        permissive: bool,
    ) -> Result<Vec<Option<GraphMutationJournalEntryWire>>, RouterError> {
        // Group uncached ids by graph canister, preserving the order requested by the caller.
        let mut by_canister: BTreeMap<Principal, Vec<MutationId>> = BTreeMap::new();
        let mut cached: HashMap<(Principal, MutationId), Option<GraphMutationJournalEntryWire>> =
            HashMap::new();
        {
            let cache = self.journal_entries.borrow();
            for (canister, mutation_id) in targets {
                let key = (*canister, *mutation_id);
                if let Some(entry) = cache.get(&key) {
                    cached.insert(key, entry.clone());
                } else {
                    by_canister.entry(*canister).or_default().push(*mutation_id);
                }
            }
        }

        let mut fetched: HashMap<(Principal, MutationId), Option<GraphMutationJournalEntryWire>> =
            HashMap::new();
        for (canister, mutation_ids) in by_canister {
            let mut cursor = 0usize;
            let mut size_hint = None;
            while cursor < mutation_ids.len() {
                let (chunk_end, next_hint) =
                    journal_batch_chunk_end(&mutation_ids, cursor, size_hint)?;
                size_hint = Some(next_hint);
                let result = get_mutation_journal_entries(
                    canister,
                    GetMutationJournalEntriesArgs {
                        mutation_ids: mutation_ids[cursor..chunk_end].to_vec(),
                    },
                )
                .await
                .map_err(|e| {
                    RouterError::InvalidArgument(format!("graph get_mutation_journal_entries: {e}"))
                })?;
                // Defensive: the Graph canister must return results in input order.
                let returned = result.entries.len();
                if returned == 0 || returned > (chunk_end - cursor) {
                    return Err(RouterError::InvalidArgument(format!(
                        "graph get_mutation_journal_entries returned {} entries for {} requested ids",
                        returned,
                        chunk_end - cursor
                    )));
                }
                for (offset, entry) in result.entries.into_iter().enumerate() {
                    let id = mutation_ids[cursor + offset];
                    fetched.insert((canister, id), entry.clone());
                    self.journal_entries
                        .borrow_mut()
                        .insert((canister, id), entry);
                }
                if let Some(next_id) = result.next {
                    // Graph stopped early; locate the next position in the request order.
                    let next_pos = mutation_ids[cursor + returned..]
                        .iter()
                        .position(|id| *id == next_id)
                        .map(|p| cursor + returned + p)
                        .unwrap_or_else(|| cursor + returned);
                    cursor = next_pos;
                } else {
                    cursor = chunk_end;
                }
            }
        }

        let mut out = Vec::with_capacity(targets.len());
        for (canister, mutation_id) in targets.iter() {
            let key = (*canister, *mutation_id);
            if let Some(entry) = cached.get(&key).or_else(|| fetched.get(&key)).cloned() {
                out.push(entry);
            } else if permissive {
                self.journal_entries.borrow_mut().insert(key, None);
                out.push(None);
            } else {
                return Err(RouterError::InvalidArgument(
                    "missing journal entry for requested mutation".into(),
                ));
            }
        }
        Ok(out)
    }

    /// Read `index_pending_min_mutation_id` once per Graph canister and cache the result.
    async fn resolve_pending_min_mutation_id(
        &self,
        graph_canister: Principal,
    ) -> Result<Option<MutationId>, RouterError> {
        {
            let cache = self.pending_min_mutation_id.borrow();
            if let Some(value) = cache.get(&graph_canister) {
                return Ok(*value);
            }
        }
        let value = index_pending_min_mutation_id(graph_canister)
            .await
            .map_err(|e| {
                RouterError::Internal(format!("graph index_pending_min_mutation_id: {e}"))
            })?;
        self.pending_min_mutation_id
            .borrow_mut()
            .insert(graph_canister, value);
        Ok(value)
    }
}

/// Find the largest measured sub-slice of `mutation_ids` starting at `offset` that still fits inside
/// the safe inter-canister request payload limit when encoded as `GetMutationJournalEntriesArgs`.
fn journal_batch_chunk_end(
    mutation_ids: &[MutationId],
    offset: usize,
    hint: Option<SizeHint>,
) -> Result<(usize, SizeHint), RouterError> {
    let remaining = mutation_ids.len().saturating_sub(offset);
    let fitted = adaptive_fitting_prefix(
        remaining,
        hint,
        SizingPolicy::inter_canister(),
        |count| {
            let candidate = gleaph_graph_kernel::plan_exec::GetMutationJournalEntriesArgs {
                mutation_ids: mutation_ids[offset..offset + count].to_vec(),
            };
            Encode!(&candidate)
                .map(|encoded| encoded.len())
                .map_err(|error| error.to_string())
        },
    )
    .map_err(|error| match error {
        FitError::Measure(detail) => {
            RouterError::InvalidArgument(format!("journal batch encode probe failed: {detail}"))
        }
        FitError::NoEntryFits {
            encoded_bytes,
            hard_limit_bytes,
        } => RouterError::InvalidArgument(format!(
            "single mutation journal request is {encoded_bytes} bytes, above the safe limit of {hard_limit_bytes}"
        )),
    })?
    .ok_or_else(|| RouterError::InvalidArgument("empty mutation journal batch".into()))?;
    Ok((
        offset + fitted.entry_count,
        SizeHint::new(fitted.entry_count),
    ))
}

pub(crate) fn build_router_block_plan(
    block: &gleaph_gql::ast::StatementBlock,
    schema: &dyn gleaph_gql::type_check::PropertySchema,
    stats: &crate::planner_stats::RouterGraphStats,
) -> Result<PhysicalPlan, RouterError> {
    use gleaph_gql_planner::{PlanBuildOptions, build_block_plan_with_schema_and_options};
    build_block_plan_with_schema_and_options(
        block,
        schema,
        PlanBuildOptions {
            stats: Some(stats),
            path_extensions: &gleaph_gql_integration::path_extension::GLEAPH_PATH_EXTENSION_HANDLER,
        },
    )
    .map_err(|e| RouterError::InvalidArgument(e.to_string()))
}

/// Lowers conditional policies for one ad-hoc execution ([ADR 0075] §5): when no
/// conditional grant applies, the input plans and blob pass through unchanged; otherwise
/// the caller's constants are substituted into the plans and a fresh blob is encoded.
///
/// `plans` are the pre-lowering (authorized) shapes; `pass_through_blob` is the blob of
/// exactly those plans. Returns the possibly-lowered plans plus their dispatch blob.
pub(crate) fn lower_for_execution(
    store: &RouterStore,
    caller: &Principal,
    graph_id: GraphId,
    mut plans: Vec<PhysicalPlan>,
    pass_through_blob: Vec<u8>,
    requires_write_path: bool,
) -> Result<(Vec<PhysicalPlan>, Vec<u8>), RouterError> {
    let ctx = crate::policy_pushdown::LoweringContext::new(store, graph_id);
    let lowered = crate::policy_pushdown::effective_policies(store, caller, graph_id)?;
    if lowered.is_empty() {
        return Ok((plans, pass_through_blob));
    }
    for plan in &mut plans {
        crate::policy_pushdown::lower_into_plan(&ctx, plan, &lowered);
    }
    let blob = encode_block_plans(&plans, requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    Ok((plans, blob))
}

fn pack_posting_hits(hits: &[PostingHit]) -> Vec<u64> {
    hits.iter()
        .map(|hit| (u64::from(hit.shard_id) << 32) | u64::from(hit.vertex_id))
        .collect()
}

/// Result of resolving fast-path vertex filters from index anchors.
#[derive(Clone, Debug, PartialEq, Eq)]
enum FastPathFilterResolution {
    /// Count all postings in the property bucket.
    Unfiltered,
    /// Count postings for these packed `(shard_id, vertex_id)` pairs only.
    Restricted(Vec<u64>),
    /// Anchor hit sets exceed the router budget; use generic shard execution.
    Oversized,
}

fn intersect_posting_hits(mut hit_sets: Vec<Vec<PostingHit>>) -> Vec<PostingHit> {
    if hit_sets.is_empty() {
        return Vec::new();
    }
    if hit_sets.len() == 1 {
        return hit_sets.pop().unwrap_or_default();
    }
    let mut sets: Vec<IntSet<u64>> = hit_sets
        .iter()
        .map(|hits| {
            hits.iter()
                .map(|hit| (u64::from(hit.shard_id) << 32) | u64::from(hit.vertex_id))
                .collect()
        })
        .collect();
    sets.sort_by_key(|set| set.len());
    let mut intersection = sets[0].clone();
    for set in sets.iter().skip(1) {
        intersection = intersection.intersection(set).copied().collect();
        if intersection.is_empty() {
            return Vec::new();
        }
    }
    intersection
        .into_iter()
        .map(|packed| PostingHit {
            shard_id: ShardId::new((packed >> 32) as u32),
            vertex_id: (packed & 0xFFFF_FFFF) as u32,
        })
        .collect()
}

async fn lookup_edge_equal_wires<I: IndexLookup + ?Sized>(
    index: &I,
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    payload_bytes: Vec<u8>,
    wire_label_ids: &[u16],
) -> Result<Vec<gleaph_graph_kernel::index::EdgePostingHit>, String> {
    if wire_label_ids.is_empty() {
        return index
            .lookup_edge_equal(physical_index_id, property_id, payload_bytes, None)
            .await;
    }
    let mut merged = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for &wire in wire_label_ids {
        for hit in index
            .lookup_edge_equal(
                physical_index_id,
                property_id,
                payload_bytes.clone(),
                Some(wire),
            )
            .await?
        {
            let key = (
                hit.shard_id,
                hit.owner_vertex_id,
                hit.label_id,
                hit.slot_index,
            );
            if seen.insert(key) {
                merged.push(hit);
            }
        }
    }
    Ok(merged)
}

/// Drain edge range pages within the domain-clamped interval, sieving wire labels exactly like
/// [`lookup_edge_equal_wires`] (ADR 0012 direction subset rule).
async fn lookup_edge_range_wires<I: IndexLookup + ?Sized>(
    index: &I,
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    range: PostingRangeRequest,
    wire_label_ids: &[u16],
) -> Result<Vec<gleaph_graph_kernel::index::EdgePostingHit>, String> {
    if wire_label_ids.is_empty() {
        return index
            .lookup_edge_range(physical_index_id, property_id, range, None)
            .await;
    }
    let mut merged = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for &wire in wire_label_ids {
        for hit in index
            .lookup_edge_range(physical_index_id, property_id, range.clone(), Some(wire))
            .await?
        {
            let key = (
                hit.shard_id,
                hit.owner_vertex_id,
                hit.label_id,
                hit.slot_index,
            );
            if seen.insert(key) {
                merged.push(hit);
            }
        }
    }
    Ok(merged)
}

async fn lookup_anchor_hits<I: IndexLookup + ?Sized>(
    index: &I,
    anchor: &IndexAnchor,
    shard_ids: &[ShardId],
    #[cfg_attr(not(feature = "batch-instr-log"), allow(unused_variables))]
    metrics: &mut SeedResolutionMetrics,
) -> Result<SeedHits, String> {
    #[cfg(feature = "batch-instr-log")]
    let remote_start = crate::current_instruction_counter();

    let result: Result<SeedHits, String> = match anchor {
        IndexAnchor::Equal(SeedProbe {
            physical_index_id,
            property_id,
            payload_bytes,
            ..
        }) => Ok(SeedHits::Vertices(
            index
                .lookup_equal(*physical_index_id, *property_id, payload_bytes.clone())
                .await?,
        )),
        IndexAnchor::EqualUnion { probes, .. } => {
            // Union of point probes with global (shard, vertex) deduplication so a
            // row matching two list elements seeds exactly once.
            let mut merged: Vec<PostingHit> = Vec::new();
            let mut seen = std::collections::BTreeSet::<(ShardId, u32)>::new();
            for probe in probes {
                for hit in index
                    .lookup_equal(
                        probe.physical_index_id,
                        probe.property_id,
                        probe.payload_bytes.clone(),
                    )
                    .await?
                {
                    if seen.insert((hit.shard_id, hit.vertex_id)) {
                        merged.push(hit);
                    }
                }
            }
            Ok(SeedHits::Vertices(merged))
        }
        IndexAnchor::EdgeEqual(crate::seed::EdgeSeedProbe {
            physical_index_id,
            property_id,
            payload_bytes,
            wire_label_ids,
            ..
        }) => Ok(SeedHits::Edges(
            lookup_edge_equal_wires(
                index,
                *physical_index_id,
                *property_id,
                payload_bytes.clone(),
                wire_label_ids,
            )
            .await?,
        )),
        IndexAnchor::EdgeEqualUnion { probes, .. } => {
            // Union of edge point probes with global (shard, owner, label, slot)
            // deduplication so an edge matching two list elements seeds exactly once.
            let mut merged: Vec<gleaph_graph_kernel::index::EdgePostingHit> = Vec::new();
            let mut seen = std::collections::BTreeSet::<(ShardId, u32, u16, u32)>::new();
            for probe in probes {
                for hit in lookup_edge_equal_wires(
                    index,
                    probe.physical_index_id,
                    probe.property_id,
                    probe.payload_bytes.clone(),
                    &probe.wire_label_ids,
                )
                .await?
                {
                    if seen.insert((
                        hit.shard_id,
                        hit.owner_vertex_id,
                        hit.label_id,
                        hit.slot_index,
                    )) {
                        merged.push(hit);
                    }
                }
            }
            Ok(SeedHits::Edges(merged))
        }
        IndexAnchor::EdgeRange(crate::seed::EdgeRangeSeedProbe {
            physical_index_id,
            property_id,
            bound_bytes,
            bound,
            wire_label_ids,
            ..
        }) => {
            // Clamp the one-sided bound to the literal's comparison-domain interval so a
            // mixed-type property cannot leak wrong-domain rows through the seeded path.
            let (low, high) = gleaph_gql::value_index_key::range_bounds_for_encoded_key(
                bound_bytes,
                bound.to_cmp(),
            )
            .map_err(|e| format!("edge range seed probe bound is not supported: {e}"))?;
            if low >= high {
                // Empty clamped interval: no posting can satisfy the comparison.
                return Ok(SeedHits::Edges(Vec::new()));
            }
            Ok(SeedHits::Edges(
                lookup_edge_range_wires(
                    index,
                    *physical_index_id,
                    *property_id,
                    PostingRangeRequest::Between { low, high },
                    wire_label_ids,
                )
                .await?,
            ))
        }
        IndexAnchor::EdgePrefix(crate::seed::EdgePrefixSeedProbe {
            physical_index_id,
            property_id,
            pattern_bytes,
            wire_label_ids,
            ..
        }) => {
            // Derive the encoded TEXT prefix interval from the stored pattern key. The
            // interval is structurally non-empty (high extends low by one sentinel byte),
            // so no empty-clamp case exists here.
            let (low, high) =
                gleaph_gql::value_index_key::text_prefix_range_bounds_for_encoded_key(
                    pattern_bytes,
                )
                .map_err(|e| format!("edge prefix seed probe pattern is not supported: {e}"))?;
            Ok(SeedHits::Edges(
                lookup_edge_range_wires(
                    index,
                    *physical_index_id,
                    *property_id,
                    PostingRangeRequest::Between { low, high },
                    wire_label_ids,
                )
                .await?,
            ))
        }
        IndexAnchor::Range(probe) => {
            // Clamp the one-sided bound to the literal's comparison-domain interval so a
            // mixed-type property cannot leak wrong-domain rows through the seeded path.
            let (low, high) = gleaph_gql::value_index_key::range_bounds_for_encoded_key(
                &probe.bound_bytes,
                probe.bound.to_cmp(),
            )
            .map_err(|e| format!("range seed probe bound is not supported: {e}"))?;
            if low >= high {
                // Empty clamped interval: no posting can satisfy the comparison.
                return Ok(SeedHits::Vertices(Vec::new()));
            }
            Ok(SeedHits::Vertices(
                index
                    .lookup_range(
                        probe.physical_index_id,
                        probe.property_id,
                        PostingRangeRequest::Between { low, high },
                    )
                    .await?,
            ))
        }
        IndexAnchor::Prefix(probe) => {
            // Derive the encoded TEXT prefix interval from the stored pattern key. The
            // interval is structurally non-empty (high extends low by one sentinel byte),
            // so no empty-clamp case exists here.
            let (low, high) =
                gleaph_gql::value_index_key::text_prefix_range_bounds_for_encoded_key(
                    &probe.pattern_bytes,
                )
                .map_err(|e| format!("prefix seed probe pattern is not supported: {e}"))?;
            Ok(SeedHits::Vertices(
                index
                    .lookup_range(
                        probe.physical_index_id,
                        probe.property_id,
                        PostingRangeRequest::Between { low, high },
                    )
                    .await?,
            ))
        }
        IndexAnchor::Intersection { specs, .. } => {
            let result = index
                .lookup_intersection(IndexIntersectionRequest {
                    specs: specs.clone(),
                })
                .await?;
            match result {
                IndexIntersectionResult::Vertices(hits) => Ok(SeedHits::Vertices(hits)),
                IndexIntersectionResult::Edges(hits) => Ok(SeedHits::Edges(hits)),
            }
        }
        IndexAnchor::Label {
            vertex_label_id, ..
        } => {
            if shard_ids.is_empty() {
                return Err("label export requires registered shards".into());
            }
            Ok(SeedHits::Vertices(
                collect_label_hits_for_shards(index, *vertex_label_id, shard_ids).await?,
            ))
        }
        IndexAnchor::LabelIntersection {
            vertex_label_ids, ..
        } => {
            if shard_ids.is_empty() {
                return Err("label intersection export requires registered shards".into());
            }
            Ok(SeedHits::Vertices(
                collect_label_intersection_hits_for_shards(index, vertex_label_ids, shard_ids)
                    .await?,
            ))
        }
    };

    #[cfg(feature = "batch-instr-log")]
    {
        metrics.remote_index_await +=
            crate::current_instruction_counter().saturating_sub(remote_start);
    }

    result
}

pub(crate) async fn resolve_seed_hits_from_anchors<I: IndexLookup + ?Sized>(
    index: &I,
    anchors: &[IndexAnchor],
    shard_ids: &[ShardId],
    preflight: Option<&PreflightContext>,
    metrics: &mut SeedResolutionMetrics,
) -> Result<SeedHits, String> {
    if anchors.is_empty() {
        return Ok(SeedHits::Vertices(Vec::new()));
    }

    let first = if let Some(ctx) = preflight {
        resolve_anchor_hits_for_shards(ctx, index, &anchors[0], shard_ids, metrics).await?
    } else {
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut _lookup_metrics = SeedResolutionMetrics::default();
        lookup_anchor_hits(index, &anchors[0], shard_ids, &mut _lookup_metrics).await?
    };

    if first.is_empty() {
        return Ok(first);
    }
    if anchors.len() == 1 {
        return Ok(first);
    }
    match first {
        SeedHits::Vertices(mut accumulated) => {
            for anchor in &anchors[1..] {
                let hits = if let Some(ctx) = preflight {
                    resolve_anchor_hits_for_shards(ctx, index, anchor, shard_ids, metrics).await?
                } else {
                    #[cfg_attr(
                        not(feature = "batch-instr-log"),
                        allow(clippy::default_constructed_unit_structs)
                    )]
                    let mut _lookup_metrics = SeedResolutionMetrics::default();
                    lookup_anchor_hits(index, anchor, shard_ids, &mut _lookup_metrics).await?
                };

                #[cfg(feature = "batch-instr-log")]
                let intersect_start = crate::current_instruction_counter();

                let SeedHits::Vertices(hits) = hits else {
                    return Err("mixed vertex and edge anchors in seed prefix".into());
                };
                accumulated = intersect_posting_hits(vec![accumulated, hits]);

                #[cfg(feature = "batch-instr-log")]
                {
                    metrics.anchor_intersection +=
                        crate::current_instruction_counter().saturating_sub(intersect_start);
                }

                if accumulated.is_empty() {
                    return Ok(SeedHits::Vertices(Vec::new()));
                }
            }
            Ok(SeedHits::Vertices(accumulated))
        }
        SeedHits::Edges(_) => Err("edge anchor cannot combine with additional anchors".into()),
    }
}

async fn resolve_anchor_hits_for_shards<I: IndexLookup + ?Sized>(
    ctx: &PreflightContext,
    index: &I,
    anchor: &IndexAnchor,
    shard_ids: &[ShardId],
    metrics: &mut SeedResolutionMetrics,
) -> Result<SeedHits, String> {
    // Label and LabelIntersection anchors already fan out to all shards in a single call,
    // so they are coalesced per (anchor, shard) only when shard_ids has one element.
    // For all other anchor kinds the index call itself covers all shards, so we cache by anchor.
    match anchor {
        IndexAnchor::Label { .. } | IndexAnchor::LabelIntersection { .. } => {
            let mut merged: Option<Vec<PostingHit>> = None;
            let mut saw_edges = false;
            for &shard_id in shard_ids {
                let hits = ctx
                    .resolve_anchor_hits(index, anchor, shard_id, metrics)
                    .await?;

                match hits {
                    SeedHits::Vertices(h) => {
                        if let Some(ref mut acc) = merged {
                            acc.extend(h);
                        } else {
                            merged = Some(h);
                        }
                    }
                    SeedHits::Edges(_) => saw_edges = true,
                }
            }
            if saw_edges {
                return Err("label anchor cannot produce edge hits".into());
            }
            Ok(SeedHits::Vertices(merged.unwrap_or_default()))
        }
        _ => {
            ctx.resolve_anchor_hits(index, anchor, ShardId::new(0), metrics)
                .await
        }
    }
}

async fn lookup_hits_for_anchor<I: IndexLookup + ?Sized>(
    index: &I,
    anchor: &IndexAnchor,
) -> Result<SeedHits, String> {
    #[cfg_attr(
        not(feature = "batch-instr-log"),
        allow(clippy::default_constructed_unit_structs)
    )]
    let mut _metrics = SeedResolutionMetrics::default();
    lookup_anchor_hits(index, anchor, &[], &mut _metrics).await
}

async fn resolve_fast_path_vertex_filter<I: IndexLookup + ?Sized>(
    index: &I,
    anchors: &[IndexAnchor],
) -> Result<FastPathFilterResolution, String> {
    use crate::federation::packed_vertices_exceed_fast_path_budget;

    if anchors.is_empty() {
        return Ok(FastPathFilterResolution::Unfiltered);
    }
    if anchors.len() == 1 {
        let hits = lookup_hits_for_anchor(index, &anchors[0]).await?;
        let SeedHits::Vertices(hits) = hits else {
            return Ok(FastPathFilterResolution::Oversized);
        };
        if posting_hits_exceed_fast_path_budget(&hits) {
            return Ok(FastPathFilterResolution::Oversized);
        }
        if hits.is_empty() {
            return Ok(FastPathFilterResolution::Restricted(Vec::new()));
        }
        return Ok(FastPathFilterResolution::Restricted(pack_posting_hits(
            &hits,
        )));
    }

    let mut sets: Vec<IntSet<u64>> = Vec::with_capacity(anchors.len());
    for anchor in anchors {
        let hits = lookup_hits_for_anchor(index, anchor).await?;
        let SeedHits::Vertices(hits) = hits else {
            return Ok(FastPathFilterResolution::Oversized);
        };
        if posting_hits_exceed_fast_path_budget(&hits) {
            return Ok(FastPathFilterResolution::Oversized);
        }
        let set = hits
            .iter()
            .map(|hit| (u64::from(hit.shard_id) << 32) | u64::from(hit.vertex_id))
            .collect();
        sets.push(set);
    }
    sets.sort_by_key(|set| set.len());
    let mut intersection = sets[0].clone();
    for set in sets.iter().skip(1) {
        intersection = intersection.intersection(set).copied().collect();
        if intersection.is_empty() {
            return Ok(FastPathFilterResolution::Restricted(Vec::new()));
        }
    }
    let packed: Vec<u64> = intersection.into_iter().collect();
    if packed_vertices_exceed_fast_path_budget(&packed) {
        return Ok(FastPathFilterResolution::Oversized);
    }
    Ok(FastPathFilterResolution::Restricted(packed))
}

fn unpack_posting_hits(packed: &[u64]) -> Vec<PostingHit> {
    packed
        .iter()
        .map(|entry| PostingHit {
            shard_id: ShardId::new((entry >> 32) as u32),
            vertex_id: (entry & 0xFFFF_FFFF) as u32,
        })
        .collect()
}

async fn execute_grouped_aggregate_fast_path<I: IndexLookup + ?Sized>(
    index: &I,
    fast_path: &AggregateIndexFastPath,
) -> Result<Option<Vec<ValuePostingCount>>, String> {
    let (label_id, property_anchors) = split_label_and_property_anchors(&fast_path.index_anchors)
        .map_err(|_| "invalid fast path anchor mix".to_string())?;

    let counts = match (label_id, property_anchors.as_slice()) {
        (None, []) => {
            index
                .count_postings_by_value(
                    fast_path.physical_index_id,
                    fast_path.property_id,
                    fast_path.min_count,
                    None,
                )
                .await?
        }
        (None, property_anchors) => {
            match resolve_fast_path_vertex_filter(index, property_anchors).await? {
                FastPathFilterResolution::Oversized => return Ok(None),
                FastPathFilterResolution::Unfiltered => {
                    return Err("property anchors required for fast path filter".into());
                }
                FastPathFilterResolution::Restricted(packed) => {
                    let filter = if packed.is_empty() {
                        None
                    } else {
                        Some(packed)
                    };
                    index
                        .count_postings_by_value(
                            fast_path.physical_index_id,
                            fast_path.property_id,
                            fast_path.min_count,
                            filter,
                        )
                        .await?
                }
            }
        }
        (Some(vertex_label_id), []) => {
            index
                .count_postings_by_value_for_label(
                    fast_path.physical_index_id,
                    fast_path.property_id,
                    vertex_label_id,
                    fast_path.min_count,
                )
                .await?
        }
        (Some(vertex_label_id), property_anchors) => {
            match resolve_fast_path_vertex_filter(index, property_anchors).await? {
                FastPathFilterResolution::Oversized => return Ok(None),
                FastPathFilterResolution::Unfiltered => {
                    return Err("property anchors required for label sieve".into());
                }
                FastPathFilterResolution::Restricted(packed) => {
                    if packed.is_empty() {
                        return Ok(Some(Vec::new()));
                    }
                    let hits = unpack_posting_hits(&packed);
                    let filtered = index.filter_hits_by_label(vertex_label_id, hits).await?;
                    if filtered.is_empty() {
                        return Ok(Some(Vec::new()));
                    }
                    let packed = pack_posting_hits(&filtered);
                    if packed_vertices_exceed_fast_path_budget(&packed) {
                        return Ok(None);
                    }
                    index
                        .count_postings_by_value(
                            fast_path.physical_index_id,
                            fast_path.property_id,
                            fast_path.min_count,
                            Some(packed),
                        )
                        .await?
                }
            }
        }
    };
    Ok(Some(counts))
}

/// Read with an explicit ADR 0029 §5 consistency contract (Phase 3).
///
/// `ReadMode::Eventual` is the default contract. `ReadMode::AtLeast(token)` enforces the
/// read-your-writes barrier: the read is served only once every shard in the token has
/// reached its label-stats and graph-index watermarks, otherwise a retryable
/// `RouterError::ProjectionLag` is returned without serving stale state.
pub async fn gql_query(
    query: String,
    params: Vec<u8>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    run_gql(
        &query,
        &params,
        GqlExecutionMode::Query,
        "gql_query",
        false,
        None,
        read_mode,
        None,
    )
    .await
}

pub async fn gql_execute_idempotent(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<GqlQueryResult, RouterError> {
    gql_execute_idempotent_with_batch(query, params, client_mutation_key).await
}

/// Classify and execute one public Router atomic insert through the existing durable ordered paths.
pub(crate) async fn atomic_insert_public(
    request: crate::types::AtomicInsertRequest,
) -> Result<crate::types::AtomicInsertResponse, RouterError> {
    let (request, public_fingerprint) = request
        .into_classified()
        .map_err(RouterError::InvalidArgument)?;
    match request {
        crate::types::ClassifiedAtomicInsertRequest::Edge(request) => {
            execute_ordered_edge_batch_classified(request, public_fingerprint).await
        }
        crate::types::ClassifiedAtomicInsertRequest::Vertex(request) => {
            execute_ordered_vertex_batch_classified(request, public_fingerprint).await
        }
        crate::types::ClassifiedAtomicInsertRequest::Mixed(request) => {
            execute_ordered_mixed_batch_classified(request, public_fingerprint).await
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CanonicalPendingReconciliationTrigger {
    ExplicitSameKeyRetry,
    BackgroundRecovery,
}

fn record_canonical_pending_reconciliation_diagnostic(
    store: &RouterStore,
    key: &ClientMutationKey,
    family: &str,
    reason: impl Into<String>,
) -> Result<(), RouterError> {
    store.record_router_mutation_last_error(
        key,
        format!(
            "ordered {family} canonical pending reconciliation: {}",
            reason.into()
        ),
    )
}

fn exact_edge_receipt_from_journal(
    entry: &GraphMutationJournalEntryWire,
    mutation_id: MutationId,
    target: &RouterOrderedEdgeBatchTargetV1,
) -> Result<GraphOrderedEdgeBatchReceiptV1, String> {
    entry.validate().map_err(|error| error.to_string())?;
    let expected_count =
        u32::try_from(target.request.items.len()).map_err(|_| "edge item count exceeds u32")?;
    let GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version,
        graph_request_fingerprint,
        logical_item_count,
    } = entry.request_identity()
    else {
        return Err("journal identity is not OrderedEdgeBatch".into());
    };
    if *canonical_encoding_version != 1
        || *graph_request_fingerprint != target.graph_request_fingerprint
        || *logical_item_count != expected_count
    {
        return Err("journal identity does not match the stored edge target".into());
    }
    if entry.mutation_id() != mutation_id
        || entry.state() != MutationJournalState::Completed
        || entry.retirement() != GraphMutationRetirementWireV1::Active
    {
        return Err("journal is not the exact active completed edge receipt".into());
    }
    if entry.allocated_vertex_ids().is_some() {
        return Err("edge journal unexpectedly carries allocated vertex ids".into());
    }
    let receipt = GraphOrderedEdgeBatchReceiptV1 {
        logical_edge_count: entry.row_count(),
        emitted_delta_first_seq: entry.emitted_delta_first_seq(),
        emitted_delta_last_seq: entry.emitted_delta_last_seq(),
        hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
    };
    receipt.validate().map_err(|error| error.to_string())?;
    if receipt.logical_edge_count != u64::from(expected_count) {
        return Err("edge receipt count does not match the stored target".into());
    }
    Ok(receipt)
}

fn exact_vertex_receipt_from_journal(
    entry: &GraphMutationJournalEntryWire,
    mutation_id: MutationId,
    target: &RouterOrderedVertexBatchTargetV1,
) -> Result<GraphOrderedVertexBatchReceiptV1, String> {
    entry.validate().map_err(|error| error.to_string())?;
    let expected_count =
        u32::try_from(target.request.items.len()).map_err(|_| "vertex item count exceeds u32")?;
    let GraphMutationRequestIdentityV1::OrderedVertexBatch {
        canonical_encoding_version,
        graph_request_fingerprint,
        logical_item_count,
    } = entry.request_identity()
    else {
        return Err("journal identity is not OrderedVertexBatch".into());
    };
    if *canonical_encoding_version != 1
        || *graph_request_fingerprint != target.graph_request_fingerprint
        || *logical_item_count != expected_count
    {
        return Err("journal identity does not match the stored vertex target".into());
    }
    if entry.mutation_id() != mutation_id
        || entry.state() != MutationJournalState::Completed
        || entry.retirement() != GraphMutationRetirementWireV1::Active
    {
        return Err("journal is not the exact active completed vertex receipt".into());
    }
    let receipt = GraphOrderedVertexBatchReceiptV1 {
        logical_vertex_count: entry.row_count(),
        emitted_delta_first_seq: entry.emitted_delta_first_seq(),
        emitted_delta_last_seq: entry.emitted_delta_last_seq(),
        hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
        allocated_vertex_ids: entry
            .allocated_vertex_ids()
            .ok_or("vertex journal is missing allocated vertex ids")?
            .to_vec(),
    };
    receipt.validate().map_err(|error| error.to_string())?;
    if receipt.logical_vertex_count != u64::from(expected_count)
        || receipt.allocated_vertex_ids.len() != expected_count as usize
    {
        return Err("vertex receipt counts do not match the stored target".into());
    }
    Ok(receipt)
}

fn mixed_target_counts(
    target: &RouterOrderedMixedBatchTargetV1,
) -> Result<(u32, u32, u32), String> {
    let operation_count = u32::try_from(target.request.operations.len())
        .map_err(|_| "mixed operation count exceeds u32")?;
    let vertex_count = target
        .request
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation,
                gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Vertex(_)
            )
        })
        .count();
    let vertex_count = u32::try_from(vertex_count).map_err(|_| "mixed vertex count exceeds u32")?;
    let edge_count = operation_count
        .checked_sub(vertex_count)
        .ok_or("mixed edge count underflow")?;
    Ok((operation_count, vertex_count, edge_count))
}

fn exact_mixed_receipt_from_journal(
    entry: &GraphMutationJournalEntryWire,
    mutation_id: MutationId,
    target: &RouterOrderedMixedBatchTargetV1,
) -> Result<GraphOrderedMixedBatchReceiptV1, String> {
    entry.validate().map_err(|error| error.to_string())?;
    let (expected_operations, expected_vertices, expected_edges) = mixed_target_counts(target)?;
    let GraphMutationRequestIdentityV1::OrderedMixedBatch {
        canonical_encoding_version,
        graph_request_fingerprint,
        logical_operation_count,
        logical_vertex_count,
        logical_edge_count,
    } = entry.request_identity()
    else {
        return Err("journal identity is not OrderedMixedBatch".into());
    };
    if *canonical_encoding_version != 1
        || *graph_request_fingerprint != target.graph_request_fingerprint
        || *logical_operation_count != expected_operations
        || *logical_vertex_count != expected_vertices
        || *logical_edge_count != expected_edges
    {
        return Err("journal identity does not match the stored mixed target".into());
    }
    if entry.mutation_id() != mutation_id
        || entry.state() != MutationJournalState::Completed
        || entry.retirement() != GraphMutationRetirementWireV1::Active
    {
        return Err("journal is not the exact active completed mixed receipt".into());
    }
    let receipt = GraphOrderedMixedBatchReceiptV1 {
        logical_operation_count: entry.row_count(),
        logical_vertex_count: u64::from(expected_vertices),
        logical_edge_count: u64::from(expected_edges),
        emitted_delta_first_seq: entry.emitted_delta_first_seq(),
        emitted_delta_last_seq: entry.emitted_delta_last_seq(),
        hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
        allocated_vertex_ids: entry
            .allocated_vertex_ids()
            .ok_or("mixed journal is missing allocated vertex ids")?
            .to_vec(),
    };
    receipt.validate().map_err(|error| error.to_string())?;
    if receipt.logical_operation_count != u64::from(expected_operations)
        || receipt.allocated_vertex_ids.len() != expected_vertices as usize
    {
        return Err("mixed receipt counts do not match the stored target".into());
    }
    Ok(receipt)
}

fn exact_edge_receipt_from_result(
    result: GraphOrderedEdgeBatchResult,
    expected_count: u32,
) -> Result<GraphOrderedEdgeBatchReceiptV1, String> {
    let GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(receipt)) = result
    else {
        return Err("ordered edge redispatch did not return a completed receipt".into());
    };
    receipt.validate().map_err(|error| error.to_string())?;
    if receipt.logical_edge_count != u64::from(expected_count) {
        return Err("ordered edge redispatch receipt count mismatch".into());
    }
    Ok(receipt)
}

fn exact_vertex_receipt_from_result(
    result: GraphOrderedVertexBatchResult,
    expected_count: u32,
) -> Result<GraphOrderedVertexBatchReceiptV1, String> {
    let GraphOrderedVertexBatchResult::V1(GraphOrderedVertexBatchResultV1::Completed(receipt)) =
        result
    else {
        return Err("ordered vertex redispatch did not return a completed receipt".into());
    };
    receipt.validate().map_err(|error| error.to_string())?;
    if receipt.logical_vertex_count != u64::from(expected_count)
        || receipt.allocated_vertex_ids.len() != expected_count as usize
    {
        return Err("ordered vertex redispatch receipt count mismatch".into());
    }
    Ok(receipt)
}

fn exact_mixed_receipt_from_result(
    result: GraphOrderedMixedBatchResult,
    expected_operations: u32,
    expected_vertices: u32,
    expected_edges: u32,
) -> Result<GraphOrderedMixedBatchReceiptV1, String> {
    let GraphOrderedMixedBatchResult::V1(GraphOrderedMixedBatchResultV1::Completed(receipt)) =
        result
    else {
        return Err("ordered mixed redispatch did not return a completed receipt".into());
    };
    receipt.validate().map_err(|error| error.to_string())?;
    if receipt.logical_operation_count != u64::from(expected_operations)
        || receipt.logical_vertex_count != u64::from(expected_vertices)
        || receipt.logical_edge_count != u64::from(expected_edges)
        || receipt.allocated_vertex_ids.len() != expected_vertices as usize
    {
        return Err("ordered mixed redispatch receipt count mismatch".into());
    }
    Ok(receipt)
}

async fn dispatch_ordered_edge_for_reconciliation(
    graph: Principal,
    args: gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs,
) -> Result<GraphOrderedEdgeBatchResult, String> {
    #[cfg(test)]
    if let Some(result) = take_test_edge_dispatch(args.clone()) {
        return result;
    }
    execute_ordered_edge_batch_on_graph(graph, args).await
}

async fn dispatch_ordered_vertex_for_reconciliation(
    graph: Principal,
    args: gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs,
) -> Result<GraphOrderedVertexBatchResult, String> {
    #[cfg(test)]
    if let Some(result) = take_test_vertex_dispatch(args.clone()) {
        return result;
    }
    execute_ordered_vertex_batch_on_graph(graph, args).await
}

async fn dispatch_ordered_mixed_for_reconciliation(
    graph: Principal,
    args: gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs,
) -> Result<GraphOrderedMixedBatchResult, String> {
    #[cfg(test)]
    if let Some(result) = take_test_mixed_dispatch(args.clone()) {
        return result;
    }
    execute_ordered_mixed_batch_on_graph(graph, args).await
}

async fn reconcile_ordered_edge_canonical_pending(
    store: &RouterStore,
    key: &ClientMutationKey,
    replay: &crate::facade::stable::label_stats::RouterOrderedEdgeBatchReplayV1,
    mutation_id: MutationId,
    trigger: CanonicalPendingReconciliationTrigger,
    preflight: Option<&PreflightContext>,
) -> Result<(), RouterError> {
    let target = &replay.target;
    let graph = target.request.target_graph_canister;
    let shard_id = target.request.target_shard_id;
    let entry = match fetch_journal_entry(preflight, graph, mutation_id, shard_id).await {
        Ok(entry) => entry,
        Err(error) => {
            return record_canonical_pending_reconciliation_diagnostic(
                store,
                key,
                "edge",
                error.to_string(),
            );
        }
    };
    if let Some(entry) = entry {
        return match exact_edge_receipt_from_journal(&entry, mutation_id, target) {
            Ok(receipt) => store.record_ordered_edge_batch_canonical_committed(
                key,
                mutation_id,
                target.graph_request_fingerprint,
                receipt,
            ),
            Err(reason) => {
                record_canonical_pending_reconciliation_diagnostic(store, key, "edge", reason)
            }
        };
    }
    if trigger == CanonicalPendingReconciliationTrigger::BackgroundRecovery {
        return record_canonical_pending_reconciliation_diagnostic(
            store,
            key,
            "edge",
            "stored Graph journal entry is absent",
        );
    }
    let expected_count = u32::try_from(target.request.items.len())
        .map_err(|_| RouterError::InvalidArgument("edge item count exceeds u32".into()))?;
    let graph_request =
        gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(target.request.clone());
    let indexed_property_catalog =
        crate::facade::stable::indexed_catalog::ordered_edge_batch_catalog(&target.request);
    let result = match dispatch_ordered_edge_for_reconciliation(
        graph,
        gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgsV1 {
                mutation_id,
                graph_request_fingerprint: target.graph_request_fingerprint,
                execution_mode: gleaph_graph_kernel::plan_exec::OrderedBatchExecutionModeV1::Atomic,
                indexed_property_catalog,
                request: graph_request,
            },
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return record_canonical_pending_reconciliation_diagnostic(store, key, "edge", error);
        }
    };
    match exact_edge_receipt_from_result(result, expected_count) {
        Ok(receipt) => store.record_ordered_edge_batch_canonical_committed(
            key,
            mutation_id,
            target.graph_request_fingerprint,
            receipt,
        ),
        Err(reason) => {
            record_canonical_pending_reconciliation_diagnostic(store, key, "edge", reason)
        }
    }
}

async fn reconcile_ordered_vertex_canonical_pending(
    store: &RouterStore,
    key: &ClientMutationKey,
    replay: &crate::facade::stable::label_stats::RouterOrderedVertexBatchReplayV1,
    mutation_id: MutationId,
    trigger: CanonicalPendingReconciliationTrigger,
    preflight: Option<&PreflightContext>,
) -> Result<(), RouterError> {
    let target = &replay.target;
    let graph = target.request.target_graph_canister;
    let shard_id = target.request.target_shard_id;
    let entry = match fetch_journal_entry(preflight, graph, mutation_id, shard_id).await {
        Ok(entry) => entry,
        Err(error) => {
            return record_canonical_pending_reconciliation_diagnostic(
                store,
                key,
                "vertex",
                error.to_string(),
            );
        }
    };
    if let Some(entry) = entry {
        return match exact_vertex_receipt_from_journal(&entry, mutation_id, target) {
            Ok(receipt) => store.record_ordered_vertex_batch_canonical_committed(
                key,
                mutation_id,
                target.graph_request_fingerprint,
                receipt,
            ),
            Err(reason) => {
                record_canonical_pending_reconciliation_diagnostic(store, key, "vertex", reason)
            }
        };
    }
    if trigger == CanonicalPendingReconciliationTrigger::BackgroundRecovery {
        return record_canonical_pending_reconciliation_diagnostic(
            store,
            key,
            "vertex",
            "stored Graph journal entry is absent",
        );
    }
    let expected_count = u32::try_from(target.request.items.len())
        .map_err(|_| RouterError::InvalidArgument("vertex item count exceeds u32".into()))?;
    let graph_request =
        gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(target.request.clone());
    let indexed_property_catalog =
        crate::facade::stable::indexed_catalog::ordered_vertex_batch_catalog(&target.request);
    let result = match dispatch_ordered_vertex_for_reconciliation(
        graph,
        gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgsV1 {
                mutation_id,
                graph_request_fingerprint: target.graph_request_fingerprint,
                execution_mode: gleaph_graph_kernel::plan_exec::OrderedBatchExecutionModeV1::Atomic,
                indexed_property_catalog,
                request: graph_request,
            },
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return record_canonical_pending_reconciliation_diagnostic(store, key, "vertex", error);
        }
    };
    match exact_vertex_receipt_from_result(result, expected_count) {
        Ok(receipt) => store.record_ordered_vertex_batch_canonical_committed(
            key,
            mutation_id,
            target.graph_request_fingerprint,
            receipt,
        ),
        Err(reason) => {
            record_canonical_pending_reconciliation_diagnostic(store, key, "vertex", reason)
        }
    }
}

async fn reconcile_ordered_mixed_canonical_pending(
    store: &RouterStore,
    key: &ClientMutationKey,
    replay: &crate::facade::stable::label_stats::RouterOrderedMixedBatchReplayV1,
    mutation_id: MutationId,
    trigger: CanonicalPendingReconciliationTrigger,
    preflight: Option<&PreflightContext>,
) -> Result<(), RouterError> {
    let target = &replay.target;
    let graph = target.request.target_graph_canister;
    let shard_id = target.request.target_shard_id;
    let entry = match fetch_journal_entry(preflight, graph, mutation_id, shard_id).await {
        Ok(entry) => entry,
        Err(error) => {
            return record_canonical_pending_reconciliation_diagnostic(
                store,
                key,
                "mixed",
                error.to_string(),
            );
        }
    };
    if let Some(entry) = entry {
        return match exact_mixed_receipt_from_journal(&entry, mutation_id, target) {
            Ok(receipt) => store.record_ordered_mixed_batch_canonical_committed(
                key,
                mutation_id,
                target.graph_request_fingerprint,
                receipt,
            ),
            Err(reason) => {
                record_canonical_pending_reconciliation_diagnostic(store, key, "mixed", reason)
            }
        };
    }
    if trigger == CanonicalPendingReconciliationTrigger::BackgroundRecovery {
        return record_canonical_pending_reconciliation_diagnostic(
            store,
            key,
            "mixed",
            "stored Graph journal entry is absent",
        );
    }
    let (expected_operations, expected_vertices, expected_edges) =
        mixed_target_counts(target).map_err(RouterError::InvalidArgument)?;
    let graph_request =
        gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest::V1(target.request.clone());
    let indexed_property_catalog =
        crate::facade::stable::indexed_catalog::ordered_mixed_batch_catalog(&target.request);
    let result = match dispatch_ordered_mixed_for_reconciliation(
        graph,
        gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgsV1 {
                mutation_id,
                graph_request_fingerprint: target.graph_request_fingerprint,
                execution_mode: gleaph_graph_kernel::plan_exec::OrderedBatchExecutionModeV1::Atomic,
                indexed_property_catalog,
                request: graph_request,
            },
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            return record_canonical_pending_reconciliation_diagnostic(store, key, "mixed", error);
        }
    };
    match exact_mixed_receipt_from_result(
        result,
        expected_operations,
        expected_vertices,
        expected_edges,
    ) {
        Ok(receipt) => store.record_ordered_mixed_batch_canonical_committed(
            key,
            mutation_id,
            target.graph_request_fingerprint,
            receipt,
        ),
        Err(reason) => {
            record_canonical_pending_reconciliation_diagnostic(store, key, "mixed", reason)
        }
    }
}

async fn reconcile_ordered_canonical_pending(
    store: &RouterStore,
    key: &ClientMutationKey,
    record: &crate::facade::stable::label_stats::RouterMutationRecord,
    trigger: CanonicalPendingReconciliationTrigger,
    preflight: Option<&PreflightContext>,
) -> Result<(), RouterError> {
    let mutation_id = record.as_v1().mutation_id;
    match record.payload() {
        RouterMutationPayloadV1::OrderedEdgeBatch(replay) => {
            reconcile_ordered_edge_canonical_pending(
                store,
                key,
                replay,
                mutation_id,
                trigger,
                preflight,
            )
            .await
        }
        RouterMutationPayloadV1::OrderedVertexBatch(replay) => {
            reconcile_ordered_vertex_canonical_pending(
                store,
                key,
                replay,
                mutation_id,
                trigger,
                preflight,
            )
            .await
        }
        RouterMutationPayloadV1::OrderedMixedBatch(replay) => {
            reconcile_ordered_mixed_canonical_pending(
                store,
                key,
                replay,
                mutation_id,
                trigger,
                preflight,
            )
            .await
        }
        _ => Ok(()),
    }
}

#[derive(Clone, Copy)]
enum OrderedAtomicInsertFamily {
    Edge,
    Vertex,
    Mixed,
}

impl OrderedAtomicInsertFamily {
    fn name(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Vertex => "vertex",
            Self::Mixed => "mixed",
        }
    }

    fn accepts(self, payload: &RouterMutationPayloadV1) -> bool {
        match self {
            Self::Edge => matches!(
                payload,
                RouterMutationPayloadV1::OrderedEdgeBatch(_)
                    | RouterMutationPayloadV1::CompletedOrderedEdgeBatch { .. }
            ),
            Self::Vertex => matches!(
                payload,
                RouterMutationPayloadV1::OrderedVertexBatch(_)
                    | RouterMutationPayloadV1::CompletedOrderedVertexBatch { .. }
            ),
            Self::Mixed => matches!(
                payload,
                RouterMutationPayloadV1::OrderedMixedBatch(_)
                    | RouterMutationPayloadV1::CompletedOrderedMixedBatch { .. }
            ),
        }
    }

    fn is_canonical_pending(self, payload: &RouterMutationPayloadV1) -> bool {
        match (self, payload) {
            (Self::Edge, RouterMutationPayloadV1::OrderedEdgeBatch(replay)) => matches!(
                replay.target.progress,
                OrderedEdgeBatchTargetProgressV1::CanonicalPending
            ),
            (Self::Vertex, RouterMutationPayloadV1::OrderedVertexBatch(replay)) => matches!(
                replay.target.progress,
                OrderedVertexBatchTargetProgressV1::CanonicalPending
            ),
            (Self::Mixed, RouterMutationPayloadV1::OrderedMixedBatch(replay)) => matches!(
                replay.target.progress,
                OrderedMixedBatchTargetProgressV1::CanonicalPending
            ),
            _ => false,
        }
    }
}

fn is_ordered_atomic_insert_record(record: &RouterMutationRecord) -> bool {
    [
        OrderedAtomicInsertFamily::Edge,
        OrderedAtomicInsertFamily::Vertex,
        OrderedAtomicInsertFamily::Mixed,
    ]
    .into_iter()
    .any(|family| family.accepts(record.payload()))
}

async fn respond_from_existing_ordered_atomic_insert(
    store: &RouterStore,
    key: &ClientMutationKey,
    record: RouterMutationRecord,
    family: OrderedAtomicInsertFamily,
) -> Result<crate::types::AtomicInsertResponse, RouterError> {
    if !family.accepts(record.payload()) {
        return Err(RouterError::Conflict(
            "client_mutation_key belongs to a different mutation kind".into(),
        ));
    }
    if family.is_canonical_pending(record.payload()) {
        reconcile_ordered_canonical_pending(
            store,
            key,
            &record,
            CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
            None,
        )
        .await?;
    }
    let record = store.router_mutation_record(key).ok_or_else(|| {
        RouterError::Internal(format!(
            "ordered {} mutation record disappeared after retry",
            family.name()
        ))
    })?;
    let encoding_key = store.graph_element_id_encoding_key(key.graph_id)?;
    Ok(crate::types::AtomicInsertResponse::from_record_with_encoding_key(&record, &encoding_key))
}

/// Admit and execute one classified ordered edge atomic insert (ADR 0049).
///
/// V1 requires one existing catalog vocabulary. Router derives the target Graph shard from the
/// encoded endpoints; the public request does not expose physical routing state.
async fn execute_ordered_edge_batch_classified(
    request: crate::types::OrderedEdgeBatchRequest,
    public_fingerprint: [u8; 32],
) -> Result<crate::types::AtomicInsertResponse, RouterError> {
    let caller = ordered_insert_caller();
    let public_item_count = match &request {
        crate::types::OrderedEdgeBatchRequest::V1(request) => request.items.len() as u32,
    };
    let client_key = match &request {
        crate::types::OrderedEdgeBatchRequest::V1(request) => request.client_mutation_key.clone(),
    };
    let logical_graph_name = match &request {
        crate::types::OrderedEdgeBatchRequest::V1(request) => request.graph_name.clone(),
    };
    let store = RouterStore::new();
    let graph_id = crate::graph_context::resolve_graph_id_or_default(
        &store,
        caller,
        logical_graph_name.as_deref(),
    )?;
    let mutation_key = mutation_key_for(caller, graph_id, &client_key);
    if let Some(record) = store
        .router_mutation_record(&mutation_key)
        .filter(is_ordered_atomic_insert_record)
    {
        let reservation = store.reserve_mutation_id_for_client_key(
            caller,
            graph_id,
            &client_key,
            public_fingerprint.to_vec(),
        )?;
        debug_assert!(!reservation.routing_owner);
        return respond_from_existing_ordered_atomic_insert(
            &store,
            &mutation_key,
            record,
            OrderedAtomicInsertFamily::Edge,
        )
        .await;
    }
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
    let endpoints = request
        .decode_same_shard_endpoints(&encoding_key)
        .map_err(RouterError::InvalidArgument)?;
    let target_shard_id = endpoints
        .first()
        .map(|(source, _)| source.shard_id)
        .ok_or_else(|| {
            RouterError::InvalidArgument("ordered edge batch has no endpoints".into())
        })?;

    let catalog_result = match &request {
        crate::types::OrderedEdgeBatchRequest::V1(request) => store.resolve_ordered_edge_catalogs(
            graph_id,
            request
                .items
                .iter()
                .map(|item| item.edge_label_name.clone()),
            request.items.iter().flat_map(|item| {
                item.initial_edge_properties
                    .iter()
                    .map(|property| property.property_name.clone())
            }),
        ),
    };
    let (resolved_labels, resolved_properties) = match catalog_result {
        Ok(catalogs) => catalogs,
        Err(error) => return Err(error),
    };
    let target_shard = match store.resolve_shard(graph_id, target_shard_id) {
        Ok(shard) => shard,
        Err(error) => return Err(error),
    };
    let graph_request = match request.to_graph_request(
        graph_id,
        target_shard_id,
        target_shard.graph_canister,
        &endpoints,
        resolved_labels.clone(),
        resolved_properties.clone(),
    ) {
        Ok(request) => request,
        Err(error) => return Err(RouterError::InvalidArgument(error)),
    };
    let graph_request_fingerprint =
        gleaph_graph_kernel::plan_exec::ordered_edge_batch_graph_request_fingerprint(
            &graph_request,
        )
        .map_err(RouterError::InvalidArgument)?;

    let reservation = store.reserve_mutation_id_for_client_key(
        caller,
        graph_id,
        &client_key,
        public_fingerprint.to_vec(),
    )?;
    if !reservation.routing_owner {
        let record = store.router_mutation_record(&mutation_key).ok_or_else(|| {
            RouterError::Internal("ordered edge mutation reservation disappeared".into())
        })?;
        return respond_from_existing_ordered_atomic_insert(
            &store,
            &mutation_key,
            record,
            OrderedAtomicInsertFamily::Edge,
        )
        .await;
    }

    let mutation_id = reservation.mutation_id;
    let target = RouterOrderedEdgeBatchTargetV1 {
        graph_request_fingerprint,
        request: match graph_request {
            gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(request) => request,
        },
        progress: OrderedEdgeBatchTargetProgressV1::CanonicalPending,
        projection_watermark: None,
    };
    if let Err(error) = store.transition_to_ordered_edge_batch(
        &mutation_key,
        mutation_id,
        RouterMutationRequestIdentityV1::OrderedEdgeBatch {
            public_fingerprint,
            public_item_count,
        },
        resolved_labels,
        resolved_properties,
        target,
    ) {
        store.abandon_router_mutation_routing_reservation(&mutation_key)?;
        return Err(error);
    }

    let graph_request = match store
        .router_mutation_record(&mutation_key)
        .and_then(|record| match record.payload() {
            RouterMutationPayloadV1::OrderedEdgeBatch(replay) => {
                Some(replay.target.request.clone())
            }
            _ => None,
        }) {
        Some(request) => gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(request),
        None => {
            return Err(RouterError::Internal(
                "ordered Graph request missing after durable transition".into(),
            ));
        }
    };
    let indexed_property_catalog = {
        let gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequest::V1(request) =
            &graph_request;
        crate::facade::stable::indexed_catalog::ordered_edge_batch_catalog(request)
    };
    let graph_result = execute_ordered_edge_batch_on_graph(
        target_shard.graph_canister,
        gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphArgsV1 {
                mutation_id,
                graph_request_fingerprint,
                execution_mode: gleaph_graph_kernel::plan_exec::OrderedBatchExecutionModeV1::Atomic,
                indexed_property_catalog,
                request: graph_request.clone(),
            },
        ),
    )
    .await
    .map_err(RouterError::Internal)?;
    let receipt = match graph_result {
        gleaph_graph_kernel::plan_exec::GraphOrderedEdgeBatchResult::V1(
            gleaph_graph_kernel::plan_exec::GraphOrderedEdgeBatchResultV1::Completed(receipt),
        ) => receipt,
        _ => {
            return Err(RouterError::InvalidArgument(
                "ordered Graph admission did not return a canonical receipt".into(),
            ));
        }
    };
    store.record_ordered_edge_batch_canonical_committed(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        receipt.clone(),
    )?;
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::fail_after_ordered_canonical_commit() {
        return Err(RouterError::Internal(
            "pocket-ic-e2e ordered canonical commit fault".into(),
        ));
    }
    store.record_ordered_edge_batch_projection_pending(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
    )?;
    advance_label_stats_projection_through(
        &store,
        graph_id,
        target_shard.graph_canister,
        target_shard_id,
        receipt.emitted_delta_last_seq,
    )
    .await?;
    store.record_ordered_edge_batch_projection_advanced(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        gleaph_graph_kernel::plan_exec::MutationTokenShard {
            shard_id: target_shard_id,
            label_stats_seq: receipt.emitted_delta_last_seq,
        },
    )?;
    store.record_ordered_edge_batch_retirement_pending(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
    )?;
    let ack = retire_ordered_mutation_on_graph(
        target_shard.graph_canister,
        gleaph_graph_kernel::plan_exec::OrderedMutationRetirementArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedMutationRetirementArgsV1 {
                mutation_id,
                graph_request_fingerprint,
            },
        ),
    )
    .await
    .map_err(RouterError::Internal)?;
    let gleaph_graph_kernel::plan_exec::OrderedMutationRetirementAck::V1(ack) = ack;
    if ack.mutation_id != mutation_id
        || ack.graph_request_fingerprint != graph_request_fingerprint
        || ack.receipt != receipt
    {
        return Err(RouterError::InvalidArgument(
            "ordered retirement acknowledgement does not match receipt".into(),
        ));
    }
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::fail_after_ordered_retirement_ack() {
        return Err(RouterError::Internal(
            "pocket-ic-e2e ordered retirement acknowledgement fault".into(),
        ));
    }
    store.record_ordered_edge_batch_retired(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        ack.receipt,
    )?;
    let record = store
        .router_mutation_record(&mutation_key)
        .ok_or_else(|| RouterError::Internal("ordered mutation record disappeared".into()))?;
    Ok(crate::types::AtomicInsertResponse::from_record_with_encoding_key(&record, &encoding_key))
}

/// Admit and execute one public ordered vertex atomic insert (ADR 0049).
///
/// V1 selects the graph's latest live shard, following ADR 0029's placement authority. The public
/// request does not expose the physical shard id; Router resolves it before the Graph call.
async fn execute_ordered_vertex_batch_classified(
    request: crate::types::OrderedVertexBatchRequest,
    public_fingerprint: [u8; 32],
) -> Result<crate::types::AtomicInsertResponse, RouterError> {
    let caller = ordered_insert_caller();
    let (client_key, graph_name, label_names, property_names) = match &request {
        crate::types::OrderedVertexBatchRequest::V1(request) => (
            request.client_mutation_key.clone(),
            request.graph_name.clone(),
            request
                .items
                .iter()
                .flat_map(|item| item.vertex_labels.iter().cloned())
                .collect::<Vec<_>>(),
            request
                .items
                .iter()
                .flat_map(|item| {
                    item.initial_properties
                        .iter()
                        .map(|property| property.property_name.clone())
                })
                .collect::<Vec<_>>(),
        ),
    };
    let public_item_count = match &request {
        crate::types::OrderedVertexBatchRequest::V1(request) => request.items.len() as u32,
    };
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let mutation_key = mutation_key_for(caller, graph_id, &client_key);
    if let Some(record) = store
        .router_mutation_record(&mutation_key)
        .filter(is_ordered_atomic_insert_record)
    {
        let reservation = store.reserve_mutation_id_for_client_key(
            caller,
            graph_id,
            &client_key,
            public_fingerprint.to_vec(),
        )?;
        debug_assert!(!reservation.routing_owner);
        return respond_from_existing_ordered_atomic_insert(
            &store,
            &mutation_key,
            record,
            OrderedAtomicInsertFamily::Vertex,
        )
        .await;
    }
    let target =
        crate::federation::latest_shard_routing(&store.list_live_shards_for_graph_id(graph_id)?)?
            .into_iter()
            .next()
            .ok_or(RouterError::ShardNotRegistered)?;
    let target_shard_id = target.shard_id;
    let target_shard = store.resolve_shard(graph_id, target_shard_id)?;
    let (resolved_labels, resolved_properties) =
        store.resolve_ordered_vertex_catalogs(graph_id, label_names, property_names)?;
    let graph_request = request
        .to_graph_request(
            graph_id,
            target_shard_id,
            target_shard.graph_canister,
            resolved_labels.clone(),
            resolved_properties.clone(),
        )
        .map_err(RouterError::InvalidArgument)?;
    let graph_request_fingerprint =
        gleaph_graph_kernel::plan_exec::ordered_vertex_batch_graph_request_fingerprint(
            &graph_request,
        )
        .map_err(RouterError::InvalidArgument)?;

    let reservation = store.reserve_mutation_id_for_client_key(
        caller,
        graph_id,
        &client_key,
        public_fingerprint.to_vec(),
    )?;
    if !reservation.routing_owner {
        let record = store.router_mutation_record(&mutation_key).ok_or_else(|| {
            RouterError::Internal("ordered vertex mutation reservation disappeared".into())
        })?;
        return respond_from_existing_ordered_atomic_insert(
            &store,
            &mutation_key,
            record,
            OrderedAtomicInsertFamily::Vertex,
        )
        .await;
    }

    let mutation_id = reservation.mutation_id;
    let target = RouterOrderedVertexBatchTargetV1 {
        graph_request_fingerprint,
        request: match graph_request {
            gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(request) => request,
        },
        progress: OrderedVertexBatchTargetProgressV1::CanonicalPending,
        projection_watermark: None,
    };
    if let Err(error) = store.transition_to_ordered_vertex_batch(
        &mutation_key,
        mutation_id,
        RouterMutationRequestIdentityV1::OrderedVertexBatch {
            public_fingerprint,
            public_item_count,
        },
        resolved_labels,
        resolved_properties,
        target,
    ) {
        store.abandon_router_mutation_routing_reservation(&mutation_key)?;
        return Err(error);
    }

    let graph_request = store
        .router_mutation_record(&mutation_key)
        .and_then(|record| match record.payload() {
            RouterMutationPayloadV1::OrderedVertexBatch(replay) => {
                Some(replay.target.request.clone())
            }
            _ => None,
        })
        .map(gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1)
        .ok_or_else(|| {
            RouterError::Internal(
                "ordered vertex Graph request missing after durable transition".into(),
            )
        })?;
    let indexed_property_catalog = {
        let gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequest::V1(request) =
            &graph_request;
        crate::facade::stable::indexed_catalog::ordered_vertex_batch_catalog(request)
    };
    let graph_result = execute_ordered_vertex_batch_on_graph(
        target_shard.graph_canister,
        gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphArgsV1 {
                mutation_id,
                graph_request_fingerprint,
                execution_mode: gleaph_graph_kernel::plan_exec::OrderedBatchExecutionModeV1::Atomic,
                indexed_property_catalog,
                request: graph_request,
            },
        ),
    )
    .await
    .map_err(RouterError::Internal)?;
    let receipt = match graph_result {
        GraphOrderedVertexBatchResult::V1(GraphOrderedVertexBatchResultV1::Completed(receipt)) => {
            receipt
        }
        _ => {
            return Err(RouterError::InvalidArgument(
                "ordered vertex Graph admission did not return a canonical receipt".into(),
            ));
        }
    };
    store.record_ordered_vertex_batch_canonical_committed(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        receipt.clone(),
    )?;
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::fail_after_ordered_canonical_commit() {
        return Err(RouterError::Internal(
            "pocket-ic-e2e ordered vertex canonical commit fault".into(),
        ));
    }
    store.record_ordered_vertex_batch_projection_pending(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
    )?;
    advance_label_stats_projection_through(
        &store,
        graph_id,
        target_shard.graph_canister,
        target_shard_id,
        receipt.emitted_delta_last_seq,
    )
    .await?;
    store.record_ordered_vertex_batch_projection_advanced(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        MutationTokenShard {
            shard_id: target_shard_id,
            label_stats_seq: receipt.emitted_delta_last_seq,
        },
    )?;
    store.record_ordered_vertex_batch_retirement_pending(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
    )?;
    let ack = retire_ordered_vertex_mutation_on_graph(
        target_shard.graph_canister,
        gleaph_graph_kernel::plan_exec::OrderedVertexMutationRetirementArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedVertexMutationRetirementArgsV1 {
                mutation_id,
                graph_request_fingerprint,
            },
        ),
    )
    .await
    .map_err(RouterError::Internal)?;
    let gleaph_graph_kernel::plan_exec::OrderedVertexMutationRetirementAck::V1(ack) = ack;
    if ack.mutation_id != mutation_id
        || ack.graph_request_fingerprint != graph_request_fingerprint
        || ack.receipt != receipt
    {
        return Err(RouterError::InvalidArgument(
            "ordered vertex retirement acknowledgement does not match receipt".into(),
        ));
    }
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::fail_after_ordered_retirement_ack() {
        return Err(RouterError::Internal(
            "pocket-ic-e2e ordered vertex retirement acknowledgement fault".into(),
        ));
    }
    store.record_ordered_vertex_batch_retired(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        ack.receipt,
    )?;
    let record = store.router_mutation_record(&mutation_key).ok_or_else(|| {
        RouterError::Internal("ordered vertex mutation record disappeared".into())
    })?;
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
    Ok(crate::types::AtomicInsertResponse::from_record_with_encoding_key(&record, &encoding_key))
}

/// Admit one mixed ordered atomic insert and persist its Graph canonical receipt. This entry point stops
/// at `CanonicalCommitted`; projection and retirement are intentionally represented as
/// non-terminal durable progress for the following lifecycle slice.
async fn execute_ordered_mixed_batch_classified(
    request: crate::types::OrderedMixedBatchRequest,
    public_fingerprint: [u8; 32],
) -> Result<crate::types::AtomicInsertResponse, RouterError> {
    let caller = ordered_insert_caller();
    let (
        client_key,
        graph_name,
        public_operation_count,
        public_vertex_count,
        public_edge_count,
        vertex_label_names,
        edge_label_names,
        property_names,
    ) = match &request {
        crate::types::OrderedMixedBatchRequest::V1(request) => {
            let public_vertex_count = request
                .operations
                .iter()
                .filter(|operation| {
                    matches!(operation, crate::types::AtomicInsertOperationV1::Vertex(_))
                })
                .count() as u32;
            let public_operation_count = request.operations.len() as u32;
            (
                request.client_mutation_key.clone(),
                request.graph_name.clone(),
                public_operation_count,
                public_vertex_count,
                public_operation_count - public_vertex_count,
                request
                    .operations
                    .iter()
                    .flat_map(|operation| match operation {
                        crate::types::AtomicInsertOperationV1::Vertex(item) => {
                            item.vertex_labels.clone()
                        }
                        crate::types::AtomicInsertOperationV1::Edge(_) => Vec::new(),
                    })
                    .collect::<Vec<_>>(),
                request
                    .operations
                    .iter()
                    .filter_map(|operation| match operation {
                        crate::types::AtomicInsertOperationV1::Vertex(_) => None,
                        crate::types::AtomicInsertOperationV1::Edge(item) => {
                            item.edge_label_name.clone()
                        }
                    })
                    .collect::<Vec<_>>(),
                request
                    .operations
                    .iter()
                    .flat_map(|operation| match operation {
                        crate::types::AtomicInsertOperationV1::Vertex(item) => item
                            .initial_properties
                            .iter()
                            .map(|property| property.property_name.clone())
                            .collect::<Vec<_>>(),
                        crate::types::AtomicInsertOperationV1::Edge(item) => item
                            .initial_edge_properties
                            .iter()
                            .map(|property| property.property_name.clone())
                            .collect::<Vec<_>>(),
                    })
                    .collect::<Vec<_>>(),
            )
        }
    };
    let store = RouterStore::new();
    let graph_id =
        crate::graph_context::resolve_graph_id_or_default(&store, caller, graph_name.as_deref())?;
    let mutation_key = mutation_key_for(caller, graph_id, &client_key);
    if let Some(record) = store
        .router_mutation_record(&mutation_key)
        .filter(is_ordered_atomic_insert_record)
    {
        let reservation = store.reserve_mutation_id_for_client_key(
            caller,
            graph_id,
            &client_key,
            public_fingerprint.to_vec(),
        )?;
        debug_assert!(!reservation.routing_owner);
        return respond_from_existing_ordered_atomic_insert(
            &store,
            &mutation_key,
            record,
            OrderedAtomicInsertFamily::Mixed,
        )
        .await;
    }
    let encoding_key = store.graph_element_id_encoding_key(graph_id)?;
    let target_shard_id = match request
        .existing_endpoint_shard(&encoding_key)
        .map_err(RouterError::InvalidArgument)?
    {
        Some(shard_id) => shard_id,
        None => {
            crate::federation::latest_shard_routing(
                &store.list_live_shards_for_graph_id(graph_id)?,
            )?
            .into_iter()
            .next()
            .ok_or(RouterError::ShardNotRegistered)?
            .shard_id
        }
    };
    let target_shard = store.resolve_shard(graph_id, target_shard_id)?;
    let (resolved_labels, resolved_properties) = store.resolve_ordered_mixed_catalogs(
        graph_id,
        vertex_label_names,
        edge_label_names,
        property_names,
    )?;
    let graph_request = request
        .to_graph_request(
            graph_id,
            target_shard_id,
            target_shard.graph_canister,
            &encoding_key,
            resolved_labels.clone(),
            resolved_properties.clone(),
        )
        .map_err(RouterError::InvalidArgument)?;
    let graph_request_fingerprint =
        gleaph_graph_kernel::plan_exec::ordered_mixed_batch_graph_request_fingerprint(
            &graph_request,
        )
        .map_err(RouterError::InvalidArgument)?;
    let reservation = store.reserve_mutation_id_for_client_key(
        caller,
        graph_id,
        &client_key,
        public_fingerprint.to_vec(),
    )?;
    if !reservation.routing_owner {
        let record = store.router_mutation_record(&mutation_key).ok_or_else(|| {
            RouterError::Internal("ordered mixed mutation reservation disappeared".into())
        })?;
        return respond_from_existing_ordered_atomic_insert(
            &store,
            &mutation_key,
            record,
            OrderedAtomicInsertFamily::Mixed,
        )
        .await;
    }
    let mutation_id = reservation.mutation_id;
    let target = RouterOrderedMixedBatchTargetV1 {
        graph_request_fingerprint,
        request: match graph_request {
            gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest::V1(request) => request,
        },
        progress: OrderedMixedBatchTargetProgressV1::CanonicalPending,
        projection_watermark: None,
    };
    if let Err(error) = store.transition_to_ordered_mixed_batch(
        &mutation_key,
        mutation_id,
        RouterMutationRequestIdentityV1::OrderedMixedBatch {
            public_fingerprint,
            public_operation_count,
            public_vertex_count,
            public_edge_count,
        },
        resolved_labels,
        resolved_properties,
        target,
    ) {
        store.abandon_router_mutation_routing_reservation(&mutation_key)?;
        return Err(error);
    }
    let graph_request = store
        .router_mutation_record(&mutation_key)
        .and_then(|record| match record.payload() {
            RouterMutationPayloadV1::OrderedMixedBatch(replay) => {
                Some(replay.target.request.clone())
            }
            _ => None,
        })
        .map(gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest::V1)
        .ok_or_else(|| {
            RouterError::Internal(
                "ordered mixed Graph request missing after durable transition".into(),
            )
        })?;
    let indexed_property_catalog = {
        let gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequest::V1(request) =
            &graph_request;
        crate::facade::stable::indexed_catalog::ordered_mixed_batch_catalog(request)
    };
    let graph_result = execute_ordered_mixed_batch_on_graph(
        target_shard.graph_canister,
        gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphArgsV1 {
                mutation_id,
                graph_request_fingerprint,
                execution_mode: gleaph_graph_kernel::plan_exec::OrderedBatchExecutionModeV1::Atomic,
                indexed_property_catalog,
                request: graph_request,
            },
        ),
    )
    .await
    .map_err(RouterError::Internal)?;
    let receipt = match graph_result {
        gleaph_graph_kernel::plan_exec::GraphOrderedMixedBatchResult::V1(
            gleaph_graph_kernel::plan_exec::GraphOrderedMixedBatchResultV1::Completed(receipt),
        ) => receipt,
        _ => {
            return Err(RouterError::InvalidArgument(
                "ordered mixed Graph admission did not return a canonical receipt".into(),
            ));
        }
    };
    store.record_ordered_mixed_batch_canonical_committed(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        receipt.clone(),
    )?;
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::fail_after_ordered_canonical_commit() {
        return Err(RouterError::Internal(
            "pocket-ic-e2e ordered mixed canonical commit fault".into(),
        ));
    }
    store.record_ordered_mixed_batch_projection_pending(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
    )?;
    advance_label_stats_projection_through(
        &store,
        graph_id,
        target_shard.graph_canister,
        target_shard_id,
        receipt.emitted_delta_last_seq,
    )
    .await?;
    store.record_ordered_mixed_batch_projection_advanced(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        MutationTokenShard {
            shard_id: target_shard_id,
            label_stats_seq: receipt.emitted_delta_last_seq,
        },
    )?;
    store.record_ordered_mixed_batch_retirement_pending(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
    )?;
    let ack = retire_ordered_mixed_mutation_on_graph(
        target_shard.graph_canister,
        gleaph_graph_kernel::plan_exec::OrderedMutationRetirementArgs::V1(
            gleaph_graph_kernel::plan_exec::OrderedMutationRetirementArgsV1 {
                mutation_id,
                graph_request_fingerprint,
            },
        ),
    )
    .await
    .map_err(RouterError::Internal)?;
    let gleaph_graph_kernel::plan_exec::OrderedMixedMutationRetirementAck::V1(ack) = ack;
    if ack.mutation_id != mutation_id
        || ack.graph_request_fingerprint != graph_request_fingerprint
        || ack.receipt != receipt
    {
        return Err(RouterError::InvalidArgument(
            "ordered mixed retirement acknowledgement does not match receipt".into(),
        ));
    }
    #[cfg(feature = "pocket-ic-e2e")]
    if crate::test_fault::fail_after_ordered_retirement_ack() {
        return Err(RouterError::Internal(
            "pocket-ic-e2e ordered mixed retirement acknowledgement fault".into(),
        ));
    }
    store.record_ordered_mixed_batch_retired(
        &mutation_key,
        mutation_id,
        graph_request_fingerprint,
        ack.receipt,
    )?;
    let record = store
        .router_mutation_record(&mutation_key)
        .ok_or_else(|| RouterError::Internal("ordered mixed mutation record disappeared".into()))?;
    Ok(crate::types::AtomicInsertResponse::from_record_with_encoding_key(&record, &encoding_key))
}

pub(crate) async fn gql_execute_idempotent_with_batch(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<GqlQueryResult, RouterError> {
    gql_execute_idempotent_with_batch_outcome(query, params, client_mutation_key, None)
        .await?
        .ok_or_else(|| RouterError::InvalidArgument(BATCH_DEFERRED_ERROR.to_string()))
}

pub(crate) async fn gql_execute_idempotent_with_batch_outcome(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
    preflight: Option<&PreflightContext>,
) -> Result<Option<GqlQueryResult>, RouterError> {
    let result = run_gql(
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_mutate",
        false,
        Some(&client_mutation_key),
        ReadMode::Eventual,
        preflight,
    )
    .await;
    // ADR 0029 Phase 4: a federated mutation that committed canonically but could not
    // finish projection inline (returned here as an error) is left non-terminal; arm the
    // recovery driver so it converges without the client retrying. Self-guarding no-op when
    // the saga already finalized.
    crate::recovery::arm_if_needed();
    match result {
        Err(RouterError::InvalidArgument(error)) if error == BATCH_DEFERRED_ERROR => Ok(None),
        Ok(result) => Ok(Some(result)),
        Err(error) => Err(error),
    }
}

/// ADR 0029 §5 (Phase 3) read barrier.
///
/// For `AtLeast(token)`, verify every shard named in the token has reached both its
/// label-stats projection cursor and its graph-index repair watermark before any read
/// shape is served. If any watermark is unmet, return a retryable
/// [`RouterError::ProjectionLag`] instead of serving a stale projection. `Eventual` is a
/// no-op.
pub(crate) async fn enforce_read_consistency(
    store: &RouterStore,
    graph_id: GraphId,
    read_mode: &ReadMode,
) -> Result<(), RouterError> {
    enforce_read_consistency_with_lookup(store, graph_id, read_mode, index_pending_min_mutation_id)
        .await
}

/// Variant of [`enforce_read_consistency`] that coalesces
/// `index_pending_min_mutation_id` lookups per Graph canister within a batch wave.
pub(crate) async fn enforce_read_consistency_with_preflight(
    store: &RouterStore,
    graph_id: GraphId,
    read_mode: &ReadMode,
    preflight: Option<&PreflightContext>,
) -> Result<(), RouterError> {
    match preflight {
        Some(ctx) => {
            enforce_read_consistency_with_lookup(
                store,
                graph_id,
                read_mode,
                |graph_canister| async move {
                    ctx.resolve_pending_min_mutation_id(graph_canister)
                        .await
                        .map_err(|e| e.to_string())
                },
            )
            .await
        }
        None => enforce_read_consistency(store, graph_id, read_mode).await,
    }
}

async fn enforce_read_consistency_with_lookup<F, Fut>(
    store: &RouterStore,
    graph_id: GraphId,
    read_mode: &ReadMode,
    mut index_lookup: F,
) -> Result<(), RouterError>
where
    F: FnMut(Principal) -> Fut,
    Fut: std::future::Future<Output = Result<Option<MutationId>, String>>,
{
    let token = match read_mode {
        ReadMode::Eventual => return Ok(()),
        ReadMode::AtLeast(token) => token,
    };

    for shard in &token.shards {
        // Label-stats watermark: the Router projection cursor must reach the shard's seq
        // for count-only read-your-writes. Resolved locally from Router stable state.
        if let Some(required) = shard.label_stats_seq {
            let current = store.label_stats_projection_cursor(graph_id, shard.shard_id);
            if current < required {
                return Err(RouterError::ProjectionLag {
                    shard_id: shard.shard_id.raw(),
                    watermark: "label_stats".into(),
                    required,
                    current,
                });
            }
        }

        // Graph-index watermark: the exact Graph-owned ordinary-work floor must have drained past
        // the token's `mutation_id`. Index-satisfied iff `None` or
        // `mutation_id < min_pending`.
        let entry = store.resolve_shard(graph_id, shard.shard_id)?;
        let min_pending = index_lookup(entry.graph_canister)
            .await
            .map_err(RouterError::Internal)?;
        if let Some(min_pending) = min_pending
            && token.mutation_id >= min_pending
        {
            return Err(RouterError::ProjectionLag {
                shard_id: shard.shard_id.raw(),
                watermark: "graph_index".into(),
                required: token.mutation_id,
                current: min_pending,
            });
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_gql(
    query: &str,
    params: &[u8],
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
    read_mode: ReadMode,
    preflight: Option<&PreflightContext>,
) -> Result<GqlQueryResult, RouterError> {
    let result = run_gql_unchecked(
        query,
        params,
        mode,
        entrypoint,
        force,
        client_mutation_key,
        read_mode,
        preflight,
    )
    .await?;
    ensure_gql_query_result_payload(&result, entrypoint)?;
    Ok(result)
}

pub(crate) fn ensure_gql_query_result_payload(
    result: &GqlQueryResult,
    entrypoint: &str,
) -> Result<(), RouterError> {
    let encoded = Encode!(result).map_err(|error| {
        RouterError::InvalidArgument(format!("{entrypoint} response encode failed: {error}"))
    })?;
    if encoded.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "{entrypoint} response exceeds the safe payload limit of {} bytes",
            gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        )));
    }
    Ok(())
}

async fn try_execute_vector_index_ddl<C, R>(
    query: &str,
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    caller: C,
    resolve_graph: R,
) -> Option<Result<GqlQueryResult, RouterError>>
where
    C: FnOnce() -> Principal,
    R: FnOnce(Principal) -> Result<GraphId, RouterError>,
{
    let ddl = crate::index_ddl::try_parse_vector(query)?;
    let caller = caller();
    let stmt = match ddl {
        Ok(s) => s,
        Err(e) => return Some(Err(RouterError::InvalidArgument(e.to_string()))),
    };
    if let Err(e) = authorize_index_ddl(&caller) {
        return Some(Err(e));
    }
    if mode == GqlExecutionMode::Query && !force {
        return Some(Err(RouterError::ExecutionPathMismatch {
            entrypoint: entrypoint.to_string(),
            program_kind: "write".to_string(),
            call_kind: "query".to_string(),
            remedy: crate::execution_path::REMEDY_WRITE_ON_QUERY.to_string(),
        }));
    }
    match resolve_graph(caller) {
        Ok(graph_id) => {
            match crate::index_catalog::execute_vector_index_ddl_for_graph(graph_id, stmt).await {
                Ok(()) => Some(Ok(GqlQueryResult::row_count_only(0))),
                Err(e) => Some(Err(e)),
            }
        }
        Err(e) => Some(Err(e)),
    }
}

/// Whether any statement in the block is an `EXPLAIN AUTHORIZATION` diagnostic
/// ([ADR 0084]); used to route standalone diagnostics and reject mixed blocks.
fn block_contains_explain_authorization(block: &gleaph_gql::ast::StatementBlock) -> bool {
    block
        .iter_statements()
        .any(|stmt| matches!(stmt, gleaph_gql::ast::Statement::ExplainAuthorization(_)))
}

/// Execute one standalone `EXPLAIN AUTHORIZATION` statement ([ADR 0084]): resolve the
/// prepared record, apply the visibility authority gate, render the coverage join, and
/// return it as one `explanation` text row per report line. Dispatches nothing.
fn execute_explain_authorization(
    caller: Principal,
    stmt: &gleaph_gql::ast::ExplainAuthorizationStatement,
) -> Result<GqlQueryResult, RouterError> {
    let store = RouterStore::new();
    let (subject, owner_mode) = match &stmt.by_principal {
        None => (caller, false),
        Some(text) => (
            Principal::from_text(text).map_err(|_| {
                RouterError::InvalidArgument(format!("invalid BY principal {text:?}"))
            })?,
            true,
        ),
    };
    let lines = crate::authz::explain_prepared_authorization(
        &store,
        &caller,
        &stmt.query_name,
        &subject,
        owner_mode,
    )?;
    let rows: Vec<BTreeMap<String, gleaph_gql::Value>> = lines
        .into_iter()
        .map(|line| BTreeMap::from([("explanation".to_string(), gleaph_gql::Value::Text(line))]))
        .collect();
    let row_count = rows.len() as u64;
    let blob = GqlWireRows::try_from_value_rows(&rows)
        .map_err(|e| RouterError::Internal(format!("explain report encode failed: {e}")))?
        .encode_blob()
        .map_err(|e| RouterError::Internal(format!("explain report encode failed: {e}")))?;
    Ok(GqlQueryResult {
        row_count,
        rows_blob: Some(blob),
        phase: None,
        token: None,
        truncated: None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_gql_unchecked(
    query: &str,
    params: &[u8],
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
    read_mode: ReadMode,
    preflight: Option<&PreflightContext>,
) -> Result<GqlQueryResult, RouterError> {
    if let Some(result) =
        try_execute_vector_index_ddl(query, mode, entrypoint, force, msg_caller, |caller| {
            let store = RouterStore::new();
            crate::graph_context::resolve_default_graph_id(&store, caller)
        })
        .await
    {
        return result;
    }

    if let Some(ddl) = crate::index_ddl::try_parse(query) {
        let caller = msg_caller();
        authorize_index_ddl(&caller)?;
        if mode == GqlExecutionMode::Query && !force {
            return Err(RouterError::ExecutionPathMismatch {
                entrypoint: entrypoint.to_string(),
                program_kind: "write".to_string(),
                call_kind: "query".to_string(),
                remedy: crate::execution_path::REMEDY_WRITE_ON_QUERY.to_string(),
            });
        }
        let statements = ddl.map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        if statements.len() != 1 {
            return Err(RouterError::InvalidArgument(
                "ordinary CREATE INDEX GQL accepts exactly one statement; use a schema migration for multiple indexes"
                    .into(),
            ));
        }
        let stmt = statements
            .into_iter()
            .next()
            .expect("single-statement guard");
        let store = RouterStore::new();
        let graph_id = crate::graph_context::resolve_default_graph_id(&store, caller)?;
        crate::index_catalog::execute_index_ddl_for_graph(graph_id, stmt).await?;
        return Ok(GqlQueryResult::row_count_only(0));
    }

    if let Some(ddl) = crate::constraint_ddl::try_parse(query) {
        let caller = msg_caller();
        authorize_index_ddl(&caller)?;
        if mode == GqlExecutionMode::Query && !force {
            return Err(RouterError::ExecutionPathMismatch {
                entrypoint: entrypoint.to_string(),
                program_kind: "write".to_string(),
                call_kind: "query".to_string(),
                remedy: crate::execution_path::REMEDY_WRITE_ON_QUERY.to_string(),
            });
        }
        // Validate syntax so malformed constraint DDL is a precise `InvalidArgument` rather than
        // an opaque `NotImplemented`.
        let stmt = ddl.map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        // ADR 0030 slice 8: CREATE CONSTRAINT is published — the enforcement lifecycle is complete and
        // Phase-6-validated through slice 7 (INSERT Try/Acquire/Confirm, DELETE/REMOVE Release, slice-6
        // recovery, slice-7 failure-injection + canbench). This is an API-surface change only: the
        // declare-on-empty store path, validation, capacity preflight, and stable layouts are unchanged.
        // ADR 0030 slice 9: DROP CONSTRAINT is published — it synchronously flips the constraint
        // `Active → Dropping` and returns; recovery's drop-drain lane drains every reservation and
        // pending effect for the dropped `ConstraintNameId`, then deletes the record (`Removed`).
        match stmt {
            crate::constraint_ddl::ConstraintDdlStatement::Create {
                constraint_name,
                if_not_exists,
                label,
                property,
            } => {
                let store = RouterStore::new();
                let graph_id = crate::graph_context::resolve_default_graph_id(&store, caller)?;
                store.create_unique_constraint(
                    graph_id,
                    &constraint_name,
                    if_not_exists,
                    &label,
                    &property,
                )?;
                return Ok(GqlQueryResult::row_count_only(0));
            }
            crate::constraint_ddl::ConstraintDdlStatement::Drop {
                constraint_name,
                if_exists,
            } => {
                let store = RouterStore::new();
                let graph_id = crate::graph_context::resolve_default_graph_id(&store, caller)?;
                store.begin_drop_unique_constraint(graph_id, &constraint_name, if_exists)?;
                // Arm recovery so the drop-drain lane converges the constraint to `Removed`.
                crate::recovery::arm_if_needed();
                return Ok(GqlQueryResult::row_count_only(0));
            }
        }
    }

    let program = parser::parse(query).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let flags = classify_program(&program);
    let caller = msg_caller();
    authorize_adhoc_gql(&caller, flags, &program)?;
    check_adhoc_execution_path(entrypoint, mode, flags, force)?;

    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing statement block".into()))?;

    // Catalog DDL only (ADR 0070): a pure `CREATE GRAPH`/`CREATE GRAPH TYPE` program must not
    // require a resolvable graph context — the very first `CREATE GRAPH` runs while no home graph
    // exists. Unregistered names are provisioned through the shared admission flow first; the
    // binding is then written at the newly allocated `GraphId`. Pre-registered names skip
    // provisioning inside the bridge and keep the binding-only path.
    if crate::facade::stable::graph_type_catalog::block_is_catalog_ddl_only(block) {
        gleaph_gql::validate::validate(&program)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        for stmt in block.iter_statements() {
            if let gleaph_gql::ast::Statement::CreateGraph(create) = stmt {
                let graph_name = gleaph_graph_catalog::object_name_key(&create.name);
                crate::provisioning::graph::create_graph_admission(caller, &graph_name).await?;
            }
        }
        crate::facade::stable::graph_type_catalog::apply_catalog_statement_block(block, query)?;
        return Ok(GqlQueryResult::row_count_only(0));
    }

    // GRANT/REVOKE (ADR 0074 §5): host control-path statements. They never dispatch to
    // shards and must not mix with other executable content; the executor validates every
    // statement before the first stable write.
    if crate::gql_grants::block_contains_authorization_modification(block) {
        if !crate::gql_grants::block_is_authorization_only(block) {
            return Err(RouterError::InvalidArgument(
                "GRANT/REVOKE statements must be issued standalone and cannot be combined with \
                 other GQL statements"
                    .into(),
            ));
        }
        gleaph_gql::validate::validate(&program)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        crate::gql_grants::execute_authorization_block(block, caller)?;
        return Ok(GqlQueryResult::row_count_only(0));
    }

    // EXPLAIN AUTHORIZATION (ADR 0084): pure diagnostic read. It dispatches nothing and
    // mutates nothing; its own authority is settled visibility over every touched graph,
    // enforced inside the explanation path with indistinguishable NotFound failures.
    if block_contains_explain_authorization(block) {
        let stmt = match &block.first {
            gleaph_gql::ast::Statement::ExplainAuthorization(stmt) if block.next.is_empty() => stmt,
            _ => {
                return Err(RouterError::InvalidArgument(
                    "EXPLAIN AUTHORIZATION must be issued standalone and cannot be combined with \
                     other GQL statements"
                        .into(),
                ));
            }
        };
        gleaph_gql::validate::validate(&program)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        return execute_explain_authorization(caller, stmt);
    }

    let store = RouterStore::new();
    let resolved = crate::graph_context::resolve_graph_context(&store, &program, caller)?;
    let seed = crate::graph_context::session_graph_seed(&store, resolved, caller);
    gleaph_gql::validate::validate_with_seed(&program, Some(&seed))
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    // ADR 0029 §5 (Phase 3): enforce the read consistency contract before serving any
    // read shape (label-count fast path, index seed, or graph-shard scan all flow through
    // here). The barrier is a no-op for `Eventual` and for the write path.
    enforce_read_consistency_with_preflight(&store, resolved.graph_id, &read_mode, preflight)
        .await?;

    // Fast path: repeated query strings inside the same ingress re-use the cached
    // parsed/planned/encoded shape. DDL/admin paths are excluded above, and the cache is
    // scoped to a single ingress so it can never outlive a catalog mutation.
    //
    // Cached plans are stored pre-policy-lowering ([ADR 0075] §5): the cache key is
    // `(caller, graph, query)`, so one caller's constants always re-resolve identically,
    // and enforcement below never sees policy-derived property demands.
    if read_mode == ReadMode::Eventual
        && let Some(ctx) = preflight
        && let Some(entry) = ctx.get_cached_plan(caller, resolved.graph_id, query)
    {
        let pmap = decode_gql_params_blob(params)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        return match entry.dispatch {
            crate::use_graph::UseGraphV2Dispatch::EffectiveGraph { plan } => {
                // Cached plans re-enforce plan-time authorization for this caller before
                // dispatch (the cache never widens what a caller may run), then lower
                // conditional policies for this execution before dispatching.
                crate::authz::enforce_data_plane_authorization(
                    &store,
                    &caller,
                    entry.dispatch_graph_id,
                    &plan,
                )?;
                let requires_write_path = plan.has_dml();
                let (plans, plan_blob) = lower_for_execution(
                    &store,
                    &caller,
                    entry.dispatch_graph_id,
                    vec![plan],
                    entry.plan_blob,
                    requires_write_path,
                )?;
                let stats = graph_stats_for(entry.dispatch_graph_id);
                dispatch_plan_blob_with_batch(
                    entry.dispatch_graph_id,
                    &plan_blob,
                    &plans,
                    &pmap,
                    params,
                    mode,
                    client_mutation_key,
                    &stats,
                    Some(ctx),
                )
                .await
            }
            crate::use_graph::UseGraphV2Dispatch::Single { graph_id, plan } => {
                crate::authz::enforce_data_plane_authorization(&store, &caller, graph_id, &plan)?;
                let requires_write_path = plan.has_dml();
                let (plans, plan_blob) = lower_for_execution(
                    &store,
                    &caller,
                    graph_id,
                    vec![plan],
                    entry.plan_blob,
                    requires_write_path,
                )?;
                let stats = graph_stats_for(graph_id);
                dispatch_plan_blob_with_batch(
                    graph_id,
                    &plan_blob,
                    &plans,
                    &pmap,
                    params,
                    mode,
                    client_mutation_key,
                    &stats,
                    Some(ctx),
                )
                .await
            }
            _ => Err(RouterError::InvalidArgument(
                "cached plan dispatch variant not supported".into(),
            )),
        };
    }

    let tx = program
        .transaction_activity
        .as_ref()
        .expect("transaction extracted above");
    let block = tx.body.as_ref().expect("statement block extracted above");

    // Mixed programs (catalog DDL plus data statements) keep the context-resolved path; their
    // schema bindings are committed before planning/dispatch. Pure DDL returned above.
    if crate::facade::stable::graph_type_catalog::block_has_catalog_ddl(block) {
        crate::facade::stable::graph_type_catalog::apply_catalog_statement_block(block, query)?;
    }

    let dispatch = crate::use_graph::resolve_ingress_dispatch(
        &store,
        &program,
        block,
        caller,
        resolved.graph_id,
    )?;
    crate::facade::stable::graph_type_catalog::validate_block_schema_for_graph(
        &dispatch.plan_block,
        &seed,
        dispatch.dispatch_graph_id,
    )?;
    let stats = graph_stats_for(dispatch.dispatch_graph_id);
    let open = NoSchema;
    let mut typed = None;
    let schema = crate::facade::stable::graph_type_catalog::property_schema_for_planning(
        dispatch.dispatch_graph_id,
        &open,
        &mut typed,
    )?;
    let plan = build_router_block_plan(&dispatch.plan_block, schema, &stats)?;
    // ADR 0074 slice 2b: plan-time data-plane enforcement. Every vertex label, edge
    // label × direction, projected property, and mutation the plan demands must be covered
    // by the caller's effective privileges (`caller ∪ PUBLIC`, ownership-derived tenancy);
    // any uncovered demand fails uniformly before dispatch.
    crate::authz::enforce_data_plane_authorization(
        &store,
        &caller,
        dispatch.dispatch_graph_id,
        &plan,
    )?;
    // ADR 0075 §5: conditional policies lower into ordinary plan machinery strictly
    // after enforcement (policies constrain outputs; they add no requirements), so the
    // dispatched shape is policy-filtered while the authorized shape stays user-authored.
    // Lowering never changes DML content (`PropertyFilter`/`IndexScan` are non-DML), so
    // the classification check below is lowering-invariant. The ingress cache keeps the
    // pre-lowering plan and blob so replay re-enforces against user-authored demands
    // only and re-resolves constants per execution.
    let cached_plan = plan.clone();
    let requires_write_path = cached_plan.has_dml();
    if requires_write_path != flags.requires_write_path() {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }
    let cached_plan_blob =
        encode_block_plans(std::slice::from_ref(&cached_plan), requires_write_path)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let (mut plans, plan_blob) = lower_for_execution(
        &store,
        &caller,
        dispatch.dispatch_graph_id,
        vec![plan],
        cached_plan_blob,
        requires_write_path,
    )?;
    let plan = plans.pop().expect("one lowered plan");

    // ADR 0029 Phase 5: a federated bundle of more than one top-level DML statement is admitted only
    // when it provably has no cross-shard read; otherwise its partial-application semantics are
    // undefined. This pre-dispatch gate (the AST is the SSOT for "how many DML statements the user
    // wrote") is the single admission point and exempts the two structurally-safe shapes:
    //   * a completely-new INSERT-only bundle (contract 1) — placed on the graph's latest shard; and
    //   * a seeded threaded bundle (contract 1/2) — one or more leading index/label anchors, no
    //     other existing-state read. When its complete row seed resolves to one shard it runs there atomically
    //     (contract 1); when it fans out to many shards it is dispatched per shard as a roll-forward
    //     saga (contract 2): each shard is atomic shard-locally, cross-shard convergence is
    //     roll-forward (no global rollback), resumed by idempotent retry / the recovery timer.
    // Any other multi-DML bundle on a federated graph is rejected here, before resolving seeds or
    // dispatching to any shard, so no accepted program has unspecified partial semantics.
    if requires_write_path && !plan.is_pure_insert() && !plan.is_single_anchor_threaded_bundle() {
        enforce_multi_dml_bundle_gate(&store, dispatch.dispatch_graph_id, block)?;
    }

    let session_current =
        crate::graph_context::session_current_after_activity(&store, &program, caller)?;
    let v2 = crate::use_graph::analyze_use_graph_v2_dispatch(
        plan,
        &store,
        caller,
        session_current,
        resolved.graph_id,
    )?;

    match &v2 {
        crate::use_graph::UseGraphV2Dispatch::EffectiveGraph { plan } => {
            if let Some(result) = crate::gql_search::try_execute_gql_search(
                plan,
                dispatch.dispatch_graph_id,
                params,
                mode,
                &stats,
                &store,
                msg_caller(),
                |target, req| async move {
                    crate::vector_sync::vector_search(target, req)
                        .await
                        .map_err(crate::state::RouterError::Internal)
                },
            )
            .await?
            {
                return Ok(result);
            }
        }
        crate::use_graph::UseGraphV2Dispatch::Single { graph_id, plan } => {
            let focused_stats = graph_stats_for(*graph_id);
            if let Some(result) = crate::gql_search::try_execute_gql_search(
                plan,
                *graph_id,
                params,
                mode,
                &focused_stats,
                &store,
                msg_caller(),
                |target, req| async move {
                    crate::vector_sync::vector_search(target, req)
                        .await
                        .map_err(crate::state::RouterError::Internal)
                },
            )
            .await?
            {
                return Ok(result);
            }
        }
        crate::use_graph::UseGraphV2Dispatch::Multi { plan, .. }
        | crate::use_graph::UseGraphV2Dispatch::Join { plan, .. } => {
            if gleaph_gql_planner::plan_contains_search(plan) {
                return Err(RouterError::InvalidArgument(
                    "GQL SEARCH is only supported for single-graph queries in this slice".into(),
                ));
            }
        }
    }

    let pmap =
        decode_gql_params_blob(params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    match v2 {
        crate::use_graph::UseGraphV2Dispatch::EffectiveGraph { plan } => {
            if let Some(ctx) = preflight {
                ctx.insert_cached_plan(
                    caller,
                    resolved.graph_id,
                    query.to_string(),
                    dispatch.dispatch_graph_id,
                    crate::use_graph::UseGraphV2Dispatch::EffectiveGraph {
                        plan: cached_plan.clone(),
                    },
                    plan_blob.clone(),
                );
            }
            dispatch_plan_blob_with_batch(
                dispatch.dispatch_graph_id,
                &plan_blob,
                std::slice::from_ref(&plan),
                &pmap,
                params,
                mode,
                client_mutation_key,
                &stats,
                preflight,
            )
            .await
        }
        crate::use_graph::UseGraphV2Dispatch::Single { graph_id, plan } => {
            let stats = graph_stats_for(graph_id);
            if let Some(ctx) = preflight {
                ctx.insert_cached_plan(
                    caller,
                    resolved.graph_id,
                    query.to_string(),
                    graph_id,
                    crate::use_graph::UseGraphV2Dispatch::Single {
                        graph_id,
                        plan: cached_plan.clone(),
                    },
                    plan_blob.clone(),
                );
            }
            dispatch_plan_blob_with_batch(
                graph_id,
                &plan_blob,
                std::slice::from_ref(&plan),
                &pmap,
                params,
                mode,
                client_mutation_key,
                &stats,
                preflight,
            )
            .await
        }
        crate::use_graph::UseGraphV2Dispatch::Multi { segments, plan } => {
            dispatch_multi_graph_use_segments(
                segments,
                &plan,
                requires_write_path,
                &pmap,
                params,
                mode,
                client_mutation_key,
            )
            .await
        }
        crate::use_graph::UseGraphV2Dispatch::Join {
            left,
            right,
            join,
            tail_ops,
            plan,
        } => {
            dispatch_use_graph_join(
                left,
                right,
                join,
                tail_ops,
                &plan,
                requires_write_path,
                &pmap,
                params,
                mode,
                client_mutation_key,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_multi_graph_use_segments(
    segments: Vec<crate::use_graph::UseGraphSegment>,
    output_plan: &PhysicalPlan,
    requires_write_path: bool,
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
) -> Result<GqlQueryResult, RouterError> {
    if requires_write_path {
        return Err(RouterError::InvalidArgument(
            "DML in multi-graph USE GRAPH is not supported".into(),
        ));
    }
    let mut merged = empty_execute_plan_result();
    for segment in segments {
        // ADR 0019 S2: each branch keeps its own GraphId context; do not treat
        // returned element ids as graph-agnostic across merged rows.
        let seg_plan = crate::use_graph::defocused_plan_from_ops(output_plan.clone(), segment.ops);
        let stats = graph_stats_for(segment.graph_id);
        let plan_blob = encode_block_plans(std::slice::from_ref(&seg_plan), requires_write_path)
            .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
        let result = dispatch_plan_blob(
            segment.graph_id,
            &plan_blob,
            std::slice::from_ref(&seg_plan),
            pmap,
            params,
            mode,
            client_mutation_key,
            &stats,
        )
        .await?;
        merge_execute_plan_result(
            &mut merged,
            ExecutePlanResult {
                row_count: result.row_count,
                rows_blob: result.rows_blob,
                hot_forward_vertices: Vec::new(),
            },
            FederatedMergeMode::UnionRows,
        )
        .map_err(RouterError::InvalidArgument)?;
    }
    Ok(GqlQueryResult::from_merged(&merged))
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_use_graph_join(
    left: crate::use_graph::UseGraphSegment,
    right: crate::use_graph::UseGraphSegment,
    join: crate::use_graph::MultiGraphJoinKind,
    tail_ops: Vec<PlanOp>,
    output_plan: &PhysicalPlan,
    requires_write_path: bool,
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
) -> Result<GqlQueryResult, RouterError> {
    if requires_write_path {
        return Err(RouterError::InvalidArgument(
            "DML in multi-graph USE GRAPH is not supported".into(),
        ));
    }
    let left_plan = crate::use_graph::defocused_plan_from_ops(output_plan.clone(), left.ops);
    let right_plan = crate::use_graph::defocused_plan_from_ops(output_plan.clone(), right.ops);
    // ADR 0019 S2: dispatch each side with its own GraphId; join merges values
    // only and does not unify physical element-id identity across graphs.
    let left_stats = graph_stats_for(left.graph_id);
    let right_stats = graph_stats_for(right.graph_id);
    let left_blob = encode_block_plans(std::slice::from_ref(&left_plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let right_blob = encode_block_plans(std::slice::from_ref(&right_plan), requires_write_path)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let left_result = dispatch_plan_blob(
        left.graph_id,
        &left_blob,
        std::slice::from_ref(&left_plan),
        pmap,
        params,
        mode,
        client_mutation_key,
        &left_stats,
    )
    .await?;
    let right_result = dispatch_plan_blob(
        right.graph_id,
        &right_blob,
        std::slice::from_ref(&right_plan),
        pmap,
        params,
        mode,
        client_mutation_key,
        &right_stats,
    )
    .await?;
    let left_wire = decode_wire_result(left_result)?;
    let right_wire = decode_wire_result(right_result)?;
    let merged = match join {
        crate::use_graph::MultiGraphJoinKind::Cartesian => {
            crate::use_graph_wire::cartesian_merge_wire_results(&left_wire, &right_wire)?
        }
        crate::use_graph::MultiGraphJoinKind::HashJoin { join_keys } => {
            crate::use_graph_wire::hash_join_wire_results(&left_wire, &right_wire, &join_keys)?
        }
    };
    let projected = crate::use_graph_wire::apply_tail_ops_wire(&merged, &tail_ops)?;
    Ok(GqlQueryResult {
        row_count: projected.rows.len() as u64,
        phase: None,
        token: None,
        truncated: None,
        rows_blob: Some(
            projected
                .encode_blob()
                .map_err(|e| RouterError::InvalidArgument(e.to_string()))?,
        ),
    })
}

fn decode_wire_result(result: GqlQueryResult) -> Result<GqlWireRows, RouterError> {
    let blob = result.rows_blob.ok_or_else(|| {
        RouterError::InvalidArgument("multi-graph branch returned no rows_blob".into())
    })?;
    GqlWireRows::decode_blob(&blob).map_err(|e| RouterError::InvalidArgument(e.to_string()))
}

/// Route and execute a plan blob (single- or multi-shard).
/// ADR 0029 Phase 5: reject a federated bundle that contains more than one top-level DML
/// statement. The block AST is the source of truth for the DML statement count; federation is
/// the live shard count of the dispatch graph. Single-shard multi-DML stays shard-local atomic
/// (Phase 1) and a single federated DML statement converges via the Phase 4 saga, so both pass.
pub(crate) fn enforce_multi_dml_bundle_gate(
    store: &RouterStore,
    graph_id: GraphId,
    block: &gleaph_gql::ast::StatementBlock,
) -> Result<(), RouterError> {
    let dml_statements = gleaph_gql::program_modification::count_dml_statements(block);
    if dml_statements <= 1 {
        return Ok(());
    }
    let shard_count = store.list_live_shards_for_graph_id(graph_id)?.len();
    if shard_count > 1 {
        return Err(RouterError::UnsupportedMultiDmlBundle {
            dml_statements: dml_statements as u32,
            shard_count: shard_count as u32,
        });
    }
    Ok(())
}

pub async fn dispatch_plan_blob(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    stats: &RouterGraphStats,
) -> Result<GqlQueryResult, RouterError> {
    dispatch_plan_blob_with_search(
        graph_id,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        stats,
        None,
        msg_caller(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_plan_blob_with_batch(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    stats: &RouterGraphStats,
    preflight: Option<&PreflightContext>,
) -> Result<GqlQueryResult, RouterError> {
    dispatch_plan_blob_with_search_and_batch(
        graph_id,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        stats,
        None,
        msg_caller(),
        preflight,
    )
    .await
}

/// Like [`dispatch_plan_blob`] but carries a Router-resolved non-leading `SEARCH` relation per
/// dispatched shard (ADR 0034 Slice 5).
#[allow(clippy::too_many_arguments)]
pub async fn dispatch_plan_blob_with_search(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    stats: &RouterGraphStats,
    resolved_search: Option<BTreeMap<ShardId, gleaph_graph_kernel::plan_exec::ResolvedSearchWire>>,
    caller: Principal,
) -> Result<GqlQueryResult, RouterError> {
    dispatch_plan_blob_with_search_and_batch(
        graph_id,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        stats,
        resolved_search,
        caller,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_plan_blob_with_search_and_batch(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    stats: &RouterGraphStats,
    resolved_search: Option<BTreeMap<ShardId, gleaph_graph_kernel::plan_exec::ResolvedSearchWire>>,
    caller: Principal,
    preflight: Option<&PreflightContext>,
) -> Result<GqlQueryResult, RouterError> {
    let store = RouterStore::new();
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    if shards.is_empty() {
        return Err(RouterError::ShardNotRegistered);
    }
    let index =
        RouterIndexLookup::from_shards(graph_id, &shards).map_err(RouterError::InvalidArgument)?;
    dispatch_plan_blob_with_index_and_batch(
        graph_id,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        &store,
        shards,
        &index,
        caller,
        stats,
        resolved_search,
        preflight,
    )
    .await
}

/// Attach the ADR 0029 federated mutation lifecycle phase to a DML result. The phase is
/// derived from the saga record, so it is only present when the caller tracks an idempotent
/// mutation via `client_mutation_key`; read queries and non-idempotent writes stay `None`.
pub(crate) fn attach_mutation_phase(
    result: GqlQueryResult,
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_mutation_key: Option<&str>,
) -> GqlQueryResult {
    let Some(key) = client_mutation_key else {
        return result;
    };
    match store.router_mutation_record(&mutation_key_for(caller, graph_id, key)) {
        Some(record) => result.with_phase(record.lifecycle_phase()),
        None => result,
    }
}

/// Whether every `ShardLocalGlobal` local claim can still be enforced fail-closed (ADR 0030 slice
/// 10): the graph must currently have **exactly one** live shard whose `(shard_id, graph_canister)`
/// identity equals each claim's recorded `owning_shard`. Any other topology — no live shard, a
/// second shard, or the same `shard_id` re-homed on a different canister — means the owning shard's
/// local table can no longer prove graph-wide uniqueness, so the mutation must be rejected. The
/// caller never falls back to FederatedTcc (which cannot see the local values). Factored out of the
/// dispatch path so this safety-critical decision is unit-testable without the dispatch machinery.
fn local_claims_enforceable(
    live_shards: &[ShardRegistryEntry],
    local_claims: &[LocalUniqueClaim],
) -> bool {
    let [sole] = live_shards else {
        return false;
    };
    local_claims.iter().all(|claim| {
        sole.shard_id == claim.owning_shard.shard_id
            && sole.graph_canister == claim.owning_shard.graph_canister
    })
}

#[allow(clippy::too_many_arguments)]
#[allow(dead_code, reason = "retained test-facing single-dispatch wrapper")]
async fn dispatch_plan_blob_with_index<I: IndexLookup + ?Sized>(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    store: &RouterStore,
    shards: Vec<ShardRegistryEntry>,
    index: &I,
    caller: Principal,
    stats: &RouterGraphStats,
    resolved_search: Option<BTreeMap<ShardId, gleaph_graph_kernel::plan_exec::ResolvedSearchWire>>,
) -> Result<GqlQueryResult, RouterError> {
    dispatch_plan_blob_with_index_and_batch(
        graph_id,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        store,
        shards,
        index,
        caller,
        stats,
        resolved_search,
        None,
    )
    .await
}

/// Outcome of preparing a mutation for batch execution.
pub(crate) enum PrepareOutcome {
    /// The mutation did not need Graph dispatch (DDL, read, or already completed).
    Early(GqlQueryResult),
    /// The mutation is ready for coalesced Graph dispatch.
    Prepared(Box<crate::batch_wave::PreparedMutation>),
}

/// Prepare a mutation for batch execution. Returns either an early result (for DDL,
/// reads, or already-completed mutations) or a [`PreparedMutation`] ready for the
/// coalesced Graph dispatch phase.
#[allow(clippy::too_many_arguments)]
async fn prepare_mutation_for_batch<I: IndexLookup + ?Sized>(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    store: &RouterStore,
    shards: Vec<ShardRegistryEntry>,
    index: &I,
    caller: Principal,
    stats: &RouterGraphStats,
    resolved_search: Option<BTreeMap<ShardId, gleaph_graph_kernel::plan_exec::ResolvedSearchWire>>,
    preflight: Option<&PreflightContext>,
    pre_reserved_mutation: Option<ClientMutationReservation>,
    persist_dispatch_envelope: bool,
) -> Result<PrepareOutcome, RouterError> {
    let mut _instr_logger = PrepareInstrLogger::new(client_mutation_key);
    let has_dml = plans.iter().any(PhysicalPlan::has_dml);
    // ADR 0029 §6 (Phase 5 contract 1): a completely-new INSERT-only write has no index anchor and
    // no existing-state reads, so it is placed on the graph's latest shard rather than rejected.
    let pure_insert_write = has_dml && plans.iter().all(PhysicalPlan::is_pure_insert);
    if mode == GqlExecutionMode::Query && !has_dml {
        if let Some(label_path) = try_label_count_telemetry_fast_path(plans, stats, store, pmap) {
            let live_count = vertex_label_live_count(store, graph_id, label_path.vertex_label_id);
            return Ok(PrepareOutcome::Early(
                gql_query_result_from_label_live_count(&label_path, live_count)
                    .map_err(RouterError::InvalidArgument)?,
            ));
        }
        if let Some(fast_path) = try_aggregate_index_fast_path(plans, stats, store, pmap)
            && let Some(counts) = execute_grouped_aggregate_fast_path(index, &fast_path)
                .await
                .map_err(RouterError::InvalidArgument)?
        {
            return Ok(PrepareOutcome::Early(
                gql_query_result_from_posting_counts(&fast_path, counts)
                    .map_err(RouterError::InvalidArgument)?,
            ));
        }
    }
    _instr_logger.mark("classify");
    let merge_mode = federated_merge_mode_from_plans(plans);
    let dispatch_plan_blob = federated_dispatch_plan_blob(shards.len(), plan_blob, plans, has_dml)
        .map_err(RouterError::InvalidArgument)?;
    let mutation_reservation = if has_dml {
        if let Some(reservation) = pre_reserved_mutation {
            Some(reservation)
        } else {
            let key = client_mutation_key.ok_or_else(|| {
                RouterError::InvalidArgument(
                    "DML execution requires client_mutation_key; use the idempotent update entrypoint"
                        .into(),
                )
            })?;
            Some(store.reserve_mutation_id_for_client_key(
                caller,
                graph_id,
                key,
                request_fingerprint(plan_blob, params, mode),
            )?)
        }
    } else {
        None
    };
    _instr_logger.mark("reserve");
    let mutation_id = mutation_reservation.map(|reservation| reservation.mutation_id);
    let mutation_key = client_mutation_key.map(|key| mutation_key_for(caller, graph_id, key));

    if has_dml && let Some(key) = mutation_key.as_ref() {
        if let Some(row_count) = store.router_mutation_completed_row_count(key) {
            return Ok(PrepareOutcome::Early(attach_mutation_phase(
                GqlQueryResult::row_count_only(row_count),
                store,
                caller,
                graph_id,
                client_mutation_key,
            )));
        }
        // A brand-new mutation has no saga record on the Router, so there is no
        // durable projection to reconcile. Skip the Graph journal reads entirely in that case.
        // Replays and retries carry an existing record and still run reconciliation.
        if let Some(record) = store.router_mutation_record(key) {
            let targets: Vec<(Principal, MutationId)> = record
                .shards()
                .iter()
                .filter(|shard| shard.completed() && !shard.projection_advanced())
                .map(|shard| (shard.graph_canister(), record.as_v1().mutation_id))
                .collect();
            if !targets.is_empty() {
                #[cfg(feature = "batch-instr-log")]
                let t0 = crate::current_instruction_counter();
                if let Some(ctx) = preflight {
                    ctx.prefetch_journal_entries(&targets).await;
                }
                #[cfg(feature = "batch-instr-log")]
                log_router_preflight(
                    "journal",
                    crate::current_instruction_counter().saturating_sub(t0),
                );
            }
            #[cfg(feature = "batch-instr-log")]
            let t0 = crate::current_instruction_counter();
            reconcile_router_mutation_projection(store, key, preflight).await?;
            #[cfg(feature = "batch-instr-log")]
            log_router_preflight(
                "reconcile",
                crate::current_instruction_counter().saturating_sub(t0),
            );
        }
        if let Some(row_count) = store.router_mutation_completed_row_count(key) {
            return Ok(PrepareOutcome::Early(attach_mutation_phase(
                GqlQueryResult::row_count_only(row_count),
                store,
                caller,
                graph_id,
                client_mutation_key,
            )));
        }
    }

    _instr_logger.mark("replay");
    let saved_record = mutation_key
        .as_ref()
        .and_then(|key| store.router_mutation_record(key));
    let mut resolved_labels = match saved_record
        .as_ref()
        .and_then(|record| record.as_v1().resolved_labels.clone())
    {
        Some(resolved_labels) => resolved_labels,
        None => {
            if let Some(ctx) = preflight
                && let Some(labels) = ctx.get_resolved_labels(graph_id, plan_blob)
            {
                labels
            } else {
                match store.resolve_plan_labels(graph_id, plans) {
                    Ok(labels) => {
                        if let Some(ctx) = preflight {
                            ctx.insert_resolved_labels(graph_id, plan_blob, labels.clone());
                        }
                        labels
                    }
                    Err(err) => {
                        release_routing_if_owner(
                            store,
                            caller,
                            graph_id,
                            client_mutation_key,
                            mutation_reservation,
                        )?;
                        return Err(err);
                    }
                }
            }
        }
    };
    _instr_logger.mark("labels");
    let mut resolved_properties = match saved_record
        .as_ref()
        .and_then(|record| record.as_v1().resolved_properties.clone())
    {
        Some(resolved_properties) => resolved_properties,
        None => {
            if let Some(ctx) = preflight
                && let Some(props) = ctx.get_resolved_properties(graph_id, plan_blob)
            {
                props
            } else {
                match store.resolve_plan_properties(graph_id, plans) {
                    Ok(props) => {
                        if let Some(ctx) = preflight {
                            ctx.insert_resolved_properties(graph_id, plan_blob, props.clone());
                        }
                        props
                    }
                    Err(err) => {
                        release_routing_if_owner(
                            store,
                            caller,
                            graph_id,
                            client_mutation_key,
                            mutation_reservation,
                        )?;
                        return Err(err);
                    }
                }
            }
        }
    };

    // ADR 0030: refuse SET writes that would touch a constrained value before the two-phase
    // acquire/release protocol exists — they would otherwise reach the canonical write unguarded and
    // could create a duplicate once `CREATE CONSTRAINT` is published. Checked before dispatch so the
    // refusal records no envelope and reserves nothing.
    _instr_logger.mark("props");
    if let Err(err) = store.reject_unsupported_constrained_writes(graph_id, plans) {
        release_routing_if_owner(
            store,
            caller,
            graph_id,
            client_mutation_key,
            mutation_reservation,
        )?;
        return Err(err);
    }

    // ADR 0030 slice 5a: detect the cross-shard uniqueness claims this INSERT makes (admission gate
    // + canonical `encoded_value`). Computed before dispatch so a rejected/over-scope constrained
    // insert never records an envelope or reserves anything.
    let planned_claims = match store.plan_unique_claims(
        graph_id,
        plans,
        pmap,
        &resolved_labels,
        &resolved_properties,
    ) {
        Ok(claims) => claims,
        Err(err) => {
            release_routing_if_owner(
                store,
                caller,
                graph_id,
                client_mutation_key,
                mutation_reservation,
            )?;
            return Err(err);
        }
    };
    // ADR 0030 slice 10: `unique_claims` are the FederatedTcc claims (Try/Acquire/Confirm);
    // `local_claims` are the ShardLocalGlobal fast-path claims (no Router reservation).
    let unique_claims = planned_claims.federated;
    let local_claims = planned_claims.local;

    // ADR 0030 slice 5b: a mutation that can delete/remove a constrained element carries the graph's
    // constrained `(label, property)` set so the shard frees each value. Release is admission-free
    // (no Try, any cardinality, any shard count). ADR 0030 slice 10 partitions it: federated values
    // free via a pinned outbox `Release` the Router reconciles by `owner_element_id`; local values
    // free directly in the owning shard's local table.
    let constrained_split = if plan_can_release(plans) {
        store.constrained_property_dispatch(graph_id)
    } else {
        ConstrainedDispatchSplit::default()
    };
    let constrained_properties = constrained_split.federated;
    let local_constrained_properties = constrained_split.local;

    // The batch preflight above awaits other mutations in the same ingress. A retry or a
    // duplicate in-flight path can complete this client key while this future is suspended, so
    // repeat the terminal replay check immediately before constructing dispatches. Without this
    // second check, a stale preflight path can dispatch a shard for a journal already compacted
    // with `completed_row_count`, and completion then reports a missing shard envelope.
    if has_dml
        && let Some(key) = mutation_key.as_ref()
        && let Some(row_count) = store.router_mutation_completed_row_count(key)
    {
        return Ok(PrepareOutcome::Early(attach_mutation_phase(
            GqlQueryResult::row_count_only(row_count),
            store,
            caller,
            graph_id,
            client_mutation_key,
        )));
    }

    _instr_logger.mark("claims");
    let mut dispatches: Vec<ShardDispatch> = if let Some(record) = saved_record.as_ref()
        && !record.shards().is_empty()
    {
        record
            .shards()
            .iter()
            .map(|shard| ShardDispatch {
                shard_id: shard.shard_id(),
                graph_canister: shard.graph_canister(),
                seed_bindings_blob: shard.seed_bindings_blob().clone(),
                resolved_search_blob: None,
            })
            .collect()
    } else {
        let seed_anchors = match SeedAnchorSet::from_plans(plans, pmap, store, stats) {
            Ok(seed_anchors) => seed_anchors,
            Err(err) => {
                release_routing_if_owner(
                    store,
                    caller,
                    graph_id,
                    client_mutation_key,
                    mutation_reservation,
                )?;
                return Err(err);
            }
        };
        let policy = sharding_policy_for(&shards);
        let shard_ids: Vec<_> = shards.iter().map(|entry| entry.shard_id).collect();
        let routings = match seed_anchors {
            Some(set) => {
                #[cfg_attr(
                    not(feature = "batch-instr-log"),
                    allow(clippy::default_constructed_unit_structs)
                )]
                let mut _scalar_seed_metrics = SeedResolutionMetrics::default();
                let hits = match resolve_seed_hits_from_anchors(
                    index,
                    set.anchors(),
                    &shard_ids,
                    preflight,
                    &mut _scalar_seed_metrics,
                )
                .await
                .map_err(RouterError::InvalidArgument)
                {
                    Ok(hits) => hits,
                    Err(err) => {
                        release_routing_if_owner(
                            store,
                            caller,
                            graph_id,
                            client_mutation_key,
                            mutation_reservation,
                        )?;
                        return Err(err);
                    }
                };
                if hits.is_empty() {
                    if let Some(key) = mutation_key.as_ref() {
                        store.record_router_mutation_completed_without_shards(
                            key,
                            resolved_labels.clone(),
                            resolved_properties.clone(),
                            0,
                        )?;
                    }
                    return Ok(PrepareOutcome::Early(attach_mutation_phase(
                        GqlQueryResult::row_count_only(0),
                        store,
                        caller,
                        graph_id,
                        client_mutation_key,
                    )));
                }
                match policy.resolve_with_hits(store, graph_id, &shards, set.routing_anchor(), hits)
                {
                    Ok(routings) => routings,
                    Err(err) => {
                        release_routing_if_owner(
                            store,
                            caller,
                            graph_id,
                            client_mutation_key,
                            mutation_reservation,
                        )?;
                        return Err(err);
                    }
                }
            }
            None if pure_insert_write => match crate::federation::latest_shard_routing(&shards) {
                Ok(routings) => routings,
                Err(err) => {
                    release_routing_if_owner(
                        store,
                        caller,
                        graph_id,
                        client_mutation_key,
                        mutation_reservation,
                    )?;
                    return Err(err);
                }
            },
            None => match policy.resolve_without_anchor(&shards) {
                Ok(routings) => routings,
                Err(err) => {
                    release_routing_if_owner(
                        store,
                        caller,
                        graph_id,
                        client_mutation_key,
                        mutation_reservation,
                    )?;
                    return Err(err);
                }
            },
        };
        routings_to_dispatches(routings)
    };

    _instr_logger.mark("routing");
    // ADR 0030 slice 5a single-shard gate. A claim's `Acquire` is attached to the one vertex its
    // single shard creates; the same claim broadcast to multiple shards would let each commit a
    // vertex with the same value. Refuse *before* recording the shard envelope — a rejection after
    // the envelope record would strand a non-terminal saga that the recovery scan keeps revisiting.
    if (!unique_claims.is_empty() || !local_claims.is_empty()) && dispatches.len() != 1 {
        release_routing_if_owner(
            store,
            caller,
            graph_id,
            client_mutation_key,
            mutation_reservation,
        )?;
        return Err(RouterError::NotImplemented(
            "INSERT under a uniqueness constraint must route to a single shard \
             (ADR 0030 slice 5a)"
                .to_string(),
        ));
    }

    // ADR 0030 slice 10: fail-closed gate for ShardLocalGlobal claims. Such a constraint's existing
    // values live ONLY in its single owning shard's local table — they are never mirrored into
    // Router reservations. So if the graph no longer has exactly that recorded owning shard as its
    // sole live shard (a second shard appeared, the shard is gone, or the live shard's canister
    // differs), the local fast path can no longer prove graph-wide uniqueness. Reject the mutation
    // fail-closed; never fall back to FederatedTcc, which would not see the local values and could
    // admit a duplicate. (The unit-2 registration guard makes this unreachable in normal operation;
    // this is the defensive backstop.)
    if !local_claims.is_empty() && !local_claims_enforceable(&shards, &local_claims) {
        release_routing_if_owner(
            store,
            caller,
            graph_id,
            client_mutation_key,
            mutation_reservation,
        )?;
        return Err(RouterError::Conflict(
            "shard-local global unique constraint can no longer be enforced: the graph's live \
             shard no longer matches the constraint's recorded owning shard; refusing \
             fail-closed (no FederatedTcc fallback)"
                .to_string(),
        ));
    }

    // ADR 0029 Phase 5 (contract 2, roll-forward bundle): a multi-DML bundle reaching dispatch on a
    // federated graph is already structurally safe (the pre-dispatch gate admits only pure-insert or
    // seeded threaded bundles, neither of which performs a cross-shard read). A pure-insert is
    // placed on one shard; a complete-row seeded bundle whose anchors fan out to many shards is
    // dispatched per shard below as a roll-forward saga — each shard atomic shard-locally, cross-shard
    // convergence roll-forward (no global rollback), resumed by idempotent retry / the recovery timer.
    if persist_dispatch_envelope && let (Some(key), Some(_)) = (mutation_key.as_ref(), mutation_id)
    {
        // Persist the envelope whenever this path has a mutation record. The store method is
        // idempotent and only fills an empty, non-terminal record, so this must not be gated on
        // the current call owning the routing lease: a retry can have a valid dispatch path while
        // still needing to repair an envelope that a prior attempt failed to persist.
        let envelope_shards = dispatches
            .iter()
            .map(|dispatch| {
                RouterMutationShardV1::new(
                    dispatch.shard_id,
                    dispatch.graph_canister,
                    dispatch.seed_bindings_blob.clone(),
                )
            })
            .collect();
        store.record_router_mutation_shards(
            key,
            resolved_labels.clone(),
            resolved_properties.clone(),
            envelope_shards,
        )?;
        if let Some(record) = store.router_mutation_record(key) {
            let v1 = record.as_v1();
            if let Some(saved_resolved_labels) = v1.resolved_labels.clone() {
                resolved_labels = saved_resolved_labels;
            }
            if let Some(saved_resolved_properties) = v1.resolved_properties.clone() {
                resolved_properties = saved_resolved_properties;
            }
            dispatches = record
                .shards()
                .iter()
                .map(|shard| ShardDispatch {
                    shard_id: shard.shard_id(),
                    graph_canister: shard.graph_canister(),
                    seed_bindings_blob: shard.seed_bindings_blob().clone(),
                    resolved_search_blob: None,
                })
                .collect();
        }
    }

    // ADR 0034 Slice 5: attach a Router-resolved non-leading SEARCH relation to each dispatched
    // shard. Shards that execute the prefix but have no local hit receive an explicit empty
    // relation, not an absent one, so the graph executor can distinguish protocol compliance from
    // a missing context.
    if let Some(search_by_shard) = resolved_search {
        let representative = search_by_shard
            .values()
            .next()
            .expect("resolved search map is non-empty");
        for dispatch in &mut dispatches {
            let wire = search_by_shard
                .get(&dispatch.shard_id)
                .cloned()
                .unwrap_or_else(|| ResolvedSearchWire {
                    binding: representative.binding.clone(),
                    output_alias: representative.output_alias.clone(),
                    vertex_hits: Vec::new(),
                });
            dispatch.resolved_search_blob =
                Some(Encode!(&wire).expect("encode resolved search relation"));
        }
    }

    let (element_id_encoding_key, indexed_properties, indexed_embeddings) = if let Some(ctx) =
        preflight
        && let Some(snapshot) = ctx.get_graph_catalog(graph_id)
    {
        (
            snapshot.element_id_encoding_key,
            snapshot.indexed_properties.clone(),
            snapshot.indexed_embeddings.clone(),
        )
    } else {
        let element_id_encoding_key = store.graph_element_id_encoding_key(graph_id)?.0;
        // ADR 0023 D1: the router (index definitions SSOT) supplies the indexed-property
        // catalog per operation so the shard never persists derived index state.
        let indexed_properties =
            crate::facade::stable::indexed_catalog::load_indexed_property_catalog(graph_id);
        // ADR 0031: the Router (vector-index definitions SSOT) supplies the indexed-embedding
        // catalog per operation, mirroring `indexed_properties`. The builder is fail-closed on
        // the dynamic per-graph gate: it exports specs only when dispatch is ready (global
        // activation flag ON and every live shard vector-attached), otherwise it is empty and
        // derived vector sync stays inert.
        let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
        let indexed_embeddings =
            crate::facade::stable::vector_index_catalog::to_indexed_embedding_catalog(
                graph_id,
                dispatch_ready,
            );
        if let Some(ctx) = preflight {
            ctx.insert_graph_catalog(
                graph_id,
                element_id_encoding_key,
                indexed_properties.clone(),
                indexed_embeddings.clone(),
            );
        }
        (
            element_id_encoding_key,
            indexed_properties,
            indexed_embeddings,
        )
    };

    // ADR 0030 slice 5a: no-`await` Try. All fallible preflight above (routing resolution, the
    // single-shard gate, envelope record, element-id key) has run, so the only step between this
    // reservation and the canonical write is the synchronous setup below — there is no fallible
    // early-return that could strand a reservation without a write. The reservation co-commits with
    // the envelope at the first dispatch `await`; it is idempotent on replay. `dispatches.len() == 1`
    // is already guaranteed by the pre-envelope gate.
    if !unique_claims.is_empty()
        && let Some(mutation_id) = mutation_id
        && let Some(key) = client_mutation_key
        && let Err(err) = store.try_reserve_unique(
            caller,
            graph_id,
            mutation_id,
            key,
            &unique_claims,
            &dispatches,
        )
    {
        release_routing_if_owner(
            store,
            caller,
            graph_id,
            client_mutation_key,
            mutation_reservation,
        )?;
        return Err(err);
    }

    // ADR 0030 slice 5a: the target(s) the dispatched `Acquire`s commit on, captured before the
    // loop consumes `dispatches`, so Confirm can read the replicated proof afterward.
    let unique_proof_targets: Vec<Principal> = if unique_claims.is_empty() {
        Vec::new()
    } else {
        dispatches
            .iter()
            .map(|dispatch| dispatch.graph_canister)
            .collect()
    };
    let dispatch_unique_claims = (!unique_claims.is_empty()).then(|| unique_claims.clone());
    let dispatch_constrained_properties =
        (!constrained_properties.is_empty()).then(|| constrained_properties.clone());
    // ADR 0030 slice 10: the ShardLocalGlobal fast-path payloads carried on the same dispatch.
    let dispatch_local_unique_claims = (!local_claims.is_empty()).then(|| {
        local_claims
            .iter()
            .map(|claim| claim.dispatch.clone())
            .collect::<Vec<_>>()
    });
    let dispatch_local_constrained_properties =
        (!local_constrained_properties.is_empty()).then(|| local_constrained_properties.clone());
    // ADR 0030 slice 5b: every target a constrained delete/remove dispatched to, so the post-commit
    // pass can read each shard's `Release` effects and reconcile them. Captured before the loop
    // consumes `dispatches`.
    let unique_release_targets: Vec<Principal> = if constrained_properties.is_empty() {
        Vec::new()
    } else {
        dispatches
            .iter()
            .map(|dispatch| dispatch.graph_canister)
            .collect()
    };

    _instr_logger.mark("envelope");
    // ADR 0030 slice 6: register the pending unique-effect discovery rows before the first dispatch
    // `await`, so they co-commit with the reservation/envelope. Any dispatch carrying `unique_claims`
    // (an `Acquire`) or `constrained_properties` (a `Release`) may pin an effect; a crash after that
    // shard's canonical write but before the inline Confirm/reconcile leaves these rows as Driver 2's
    // only durable handle back to the pinned canister (the inline happy path runs first; Driver 2
    // removes a row only after the shard re-enumerates empty).
    if let Some(mutation_id) = mutation_id
        && let Some(key) = client_mutation_key
        && (!unique_claims.is_empty() || !constrained_properties.is_empty())
    {
        let client_key = crate::facade::stable::label_stats::ClientMutationKey::new(
            caller,
            graph_id,
            key.to_string(),
        );
        for dispatch in &dispatches {
            store.register_pending_unique_effect(
                graph_id,
                mutation_id,
                dispatch.shard_id,
                dispatch.graph_canister,
                client_key.clone(),
            );
        }
    }

    // ADR 0030 slice 7 (failure injection): trap after the no-`await` Try, before the first dispatch
    // `await`. The reservation/envelope co-commit only at that `await`, so this trap must roll them
    // back with the message — proving Try leaves no stranded reservation on a pre-dispatch crash.
    #[cfg(feature = "pocket-ic-e2e")]
    crate::test_fault::maybe_trap_after_try();

    // Aggregate only dispatches targeting the same Graph canister. The batch endpoint returns one
    // outcome per operation, so the existing per-shard journal/recovery logic below remains the
    // source of truth for mutation lifecycle transitions.
    Ok(PrepareOutcome::Prepared(Box::new(
        crate::batch_wave::PreparedMutation {
            has_dml,
            merge_mode,
            dispatches,
            mutation_id,
            unique_claims: dispatch_unique_claims.clone(),
            unique_proof_targets: unique_proof_targets.clone(),
            constrained_properties: dispatch_constrained_properties.clone(),
            unique_release_targets: unique_release_targets.clone(),
            local_unique_claims: dispatch_local_unique_claims.clone(),
            local_constrained_properties: dispatch_local_constrained_properties.clone(),
            indexed_properties: indexed_properties.clone(),
            indexed_embeddings: indexed_embeddings.clone(),
            element_id_encoding_key: gleaph_graph_kernel::federation::ElementIdEncodingKey(
                element_id_encoding_key,
            ),
            resolved_labels: resolved_labels.clone(),
            resolved_properties: resolved_properties.clone(),
            plan_blob: dispatch_plan_blob.clone(),
            pmap: pmap.clone(),
            params: params.to_vec(),
            mode,
            plans: plans.to_vec(),
        },
    )))
}

/// Execute a single prepared mutation (the Graph dispatch and post-processing phases).
async fn execute_prepared_mutation(
    prepared: crate::batch_wave::PreparedMutation,
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_mutation_key: Option<&str>,
    preflight: Option<&PreflightContext>,
) -> Result<GqlQueryResult, RouterError> {
    let mutation_key = client_mutation_key.map(|key| mutation_key_for(caller, graph_id, key));
    let mode = prepared.mode;
    let has_dml = prepared.has_dml;
    let mutation_id = prepared.mutation_id;
    let unique_claims = prepared.unique_claims.clone().unwrap_or_default();
    let unique_proof_targets = prepared.unique_proof_targets.clone();
    let constrained_properties = prepared.constrained_properties.clone().unwrap_or_default();
    let unique_release_targets = prepared.unique_release_targets.clone();
    let merge_mode = prepared.merge_mode.clone();
    let plans = prepared.plans.clone();
    let pmap = prepared.pmap.clone();
    let element_id_encoding_key = prepared.element_id_encoding_key.0;

    let build_execute_args =
        |dispatch: &ShardDispatch| gleaph_graph_kernel::plan_exec::ExecutePlanArgs {
            target_shard_id: dispatch.shard_id,
            element_id_encoding_key,
            mutation_id,
            plan_blob: prepared.plan_blob.clone(),
            params_blob: prepared.params.clone(),
            mode,
            seed_bindings_blob: dispatch.seed_bindings_blob.clone(),
            resolved_labels: Some(prepared.resolved_labels.clone()),
            resolved_properties: Some(prepared.resolved_properties.clone()),
            indexed_properties: Some(prepared.indexed_properties.clone()),
            unique_claims: prepared.unique_claims.clone(),
            constrained_properties: prepared.constrained_properties.clone(),
            local_unique_claims: prepared.local_unique_claims.clone(),
            local_constrained_properties: prepared.local_constrained_properties.clone(),
            indexed_embeddings: Some(prepared.indexed_embeddings.clone()),
            resolved_search_blob: dispatch.resolved_search_blob.clone(),
        };

    #[cfg(feature = "batch-instr-log")]
    let graph_request_build_start = crate::current_instruction_counter();

    let dispatch_groups = group_dispatches_by_graph(prepared.dispatches);
    let mut dispatch_results: Vec<BatchDispatchResult> = Vec::new();

    // Dispatch groups in parallel across target Graph canisters. Within a single
    // canister we issue sequential dynamic chunks so the message size and Graph
    // instruction budget stay safe.
    let mut group_futures: Vec<
        futures::future::BoxFuture<Result<Vec<BatchDispatchResult>, RouterError>>,
    > = Vec::new();
    for group in dispatch_groups {
        group_futures.push(Box::pin(async {
            let mut results = Vec::new();
            let group_graph = group[0].graph_canister;
            if mode == GqlExecutionMode::Query || group.len() == 1 {
                for dispatch in group {
                    let args = build_execute_args(&dispatch);
                    results.push((
                        dispatch.clone(),
                        Some(execute_plan_on_graph(group_graph, args).await),
                    ));
                }
            } else {
                let mut remaining = group;
                let mut size_hint = None;
                while !remaining.is_empty() {
                    // Between-chunk budget guard (ADR 0042 between-wave check): the whole
                    // update message shares one call-context instruction ceiling, so before
                    // starting another chunk the Router must be able to fit one more chunk
                    // round plus its finalization reserve under the 40B cap. Deferred
                    // operations surface as per-operation errors and are completed by
                    // resubmitting the mutation (journal reconciliation is idempotent).
                    if !graph_batch_chunk_within_update_budget(
                        crate::current_instruction_counter(),
                    ) {
                        let err = format!(
                            "router update instruction budget guard stopped chunk dispatch: {} operation(s) deferred; resubmit to complete",
                            remaining.len()
                        );
                        results
                            .extend(remaining.drain(..).map(|d| (d, Some(Err(err.clone())))));
                        break;
                    }
                    let (chunk_len, next_hint) = graph_batch_chunk_len_for_dispatches(
                        &remaining,
                        &build_execute_args,
                        size_hint,
                    )?;
                    size_hint = Some(next_hint);
                    let chunk: Vec<ShardDispatch> = remaining.drain(..chunk_len).collect();
                    let args = gleaph_graph_kernel::plan_exec::ExecutePlanBatchArgs {
                        operations: chunk.iter().map(&build_execute_args).collect(),
                        mode: gleaph_graph_kernel::plan_exec::ExecutePlanBatchMode::Dynamic,
                    };
                    match execute_plan_batch_on_graph(group_graph, args).await {
                        Ok(batch) if batch.results.len() == chunk.len() => {
                            results
                                .extend(chunk.into_iter().zip(batch.results.into_iter().map(Some)));
                        }
                        Ok(batch) => {
                            let err = format!(
                                "graph batch returned {} results for {} operations",
                                batch.results.len(),
                                chunk.len()
                            );
                            results.extend(chunk.into_iter().map(|d| (d, Some(Err(err.clone())))));
                        }
                        Err(err) => {
                            results.extend(chunk.into_iter().map(|d| (d, Some(Err(err.clone())))));
                        }
                    }
                }
            }
            Ok(results)
        }));
    }

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "graph_request_build",
        crate::current_instruction_counter().saturating_sub(graph_request_build_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let graph_await_start = crate::current_instruction_counter();

    for group_result in futures::future::join_all(group_futures).await {
        dispatch_results.extend(group_result?);
    }

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "graph_await",
        crate::current_instruction_counter().saturating_sub(graph_await_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let post_dispatch_start = crate::current_instruction_counter();

    let mut merged = empty_execute_plan_result();
    // ADR 0029 Phase 2: accumulate per-shard read-your-writes watermarks for the mutation
    // token as each shard completes (built live so it survives record compaction).
    let mut token_shards: Vec<MutationTokenShard> = Vec::new();

    // Coalesce per-wave mutation-journal reads. Dispatch already returned, so any
    // entries that exist are durable; missing entries are cached as `None` and handled by the
    // per-shard recovery/projection logic below.
    #[cfg(feature = "batch-instr-log")]
    let journal_read_start = crate::current_instruction_counter();
    if let (Some(ctx), Some(mutation_id)) = (preflight, mutation_id) {
        let mut seen = HashSet::new();
        let targets: Vec<(Principal, MutationId)> = dispatch_results
            .iter()
            .filter_map(|(dispatch, _)| {
                if seen.insert(dispatch.graph_canister) {
                    Some((dispatch.graph_canister, mutation_id))
                } else {
                    None
                }
            })
            .collect();
        if !targets.is_empty() {
            ctx.resolve_journal_entries(&targets).await?;
        }
    }
    #[cfg(feature = "batch-instr-log")]
    log_router_preflight(
        "post_dispatch_journal",
        crate::current_instruction_counter().saturating_sub(journal_read_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let result_regroup_merge_start = crate::current_instruction_counter();

    for (dispatch, result) in dispatch_results {
        let result =
            result.ok_or_else(|| RouterError::InvalidArgument(BATCH_DEFERRED_ERROR.to_string()))?;
        let result = match result {
            Ok(result) => result,
            Err(err) => {
                if let Some(mutation_id) = mutation_id
                    && let Some(entry) = recover_mutation_outcome(
                        store,
                        graph_id,
                        dispatch.graph_canister,
                        dispatch.shard_id,
                        mutation_id,
                        preflight,
                    )
                    .await?
                    && matches!(entry.state(), MutationJournalState::Completed)
                {
                    if has_dml {
                        crate::bulk_ingest_finalize::maybe_finalize_hot_vertices_after_dml(
                            dispatch.graph_canister,
                            dispatch.shard_id,
                            &plans,
                            &entry.hot_forward_vertices().to_vec(),
                        )
                        .await?;
                    }
                    merge_execute_plan_result(
                        &mut merged,
                        gleaph_graph_kernel::plan_exec::ExecutePlanResult {
                            row_count: entry.row_count(),
                            rows_blob: None,
                            hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
                        },
                        merge_mode.clone(),
                    )
                    .map_err(RouterError::InvalidArgument)?;
                    if let Some(key) = mutation_key.as_ref() {
                        store.record_router_mutation_shard_completed(
                            key,
                            dispatch.shard_id,
                            entry.row_count(),
                        )?;
                        store.record_router_mutation_shard_projection_advanced(
                            key,
                            dispatch.shard_id,
                        )?;
                    }
                    token_shards.push(MutationTokenShard {
                        shard_id: dispatch.shard_id,
                        label_stats_seq: entry.emitted_delta_last_seq(),
                    });
                    continue;
                }
                if let Some(detail) = err
                    .strip_prefix(gleaph_graph_kernel::federation::UNIQUENESS_VIOLATION_WIRE_PREFIX)
                {
                    return Err(RouterError::UniquenessViolation(detail.to_string()));
                }
                return Err(RouterError::InvalidArgument(err));
            }
        };
        if let Some(mutation_id) = mutation_id {
            let entry = advance_mutation_label_stats_projection(
                store,
                graph_id,
                dispatch.graph_canister,
                dispatch.shard_id,
                mutation_id,
                preflight,
            )
            .await?;
            token_shards.push(MutationTokenShard {
                shard_id: dispatch.shard_id,
                label_stats_seq: entry.emitted_delta_last_seq(),
            });
        }
        if has_dml {
            crate::bulk_ingest_finalize::maybe_finalize_hot_vertices_after_dml(
                dispatch.graph_canister,
                dispatch.shard_id,
                &plans,
                &result.hot_forward_vertices,
            )
            .await?;
        }
        if let Some(key) = mutation_key.as_ref() {
            store.record_router_mutation_shard_completed(
                key,
                dispatch.shard_id,
                result.row_count,
            )?;
            store.record_router_mutation_shard_projection_advanced(key, dispatch.shard_id)?;
        }
        merge_execute_plan_result(&mut merged, result, merge_mode.clone())
            .map_err(RouterError::InvalidArgument)?;
    }

    if let Some(mutation_id) = mutation_id
        && !unique_claims.is_empty()
    {
        #[cfg(feature = "pocket-ic-e2e")]
        crate::test_fault::maybe_trap_before_confirm();

        confirm_unique_reservations(
            store,
            graph_id,
            mutation_id,
            &unique_claims,
            &unique_proof_targets,
        )
        .await;
    }

    if let Some(mutation_id) = mutation_id
        && !constrained_properties.is_empty()
    {
        reconcile_unique_releases(store, graph_id, mutation_id, &unique_release_targets).await;
    }

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "result_regroup_merge",
        crate::current_instruction_counter().saturating_sub(result_regroup_merge_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let hot_vertex_finalize_start = crate::current_instruction_counter();

    if let Some(mutation_id) = mutation_id
        && !unique_claims.is_empty()
    {
        #[cfg(feature = "pocket-ic-e2e")]
        crate::test_fault::maybe_trap_before_confirm();

        confirm_unique_reservations(
            store,
            graph_id,
            mutation_id,
            &unique_claims,
            &unique_proof_targets,
        )
        .await;
    }

    if let Some(mutation_id) = mutation_id
        && !constrained_properties.is_empty()
    {
        reconcile_unique_releases(store, graph_id, mutation_id, &unique_release_targets).await;
    }

    if let FederatedMergeMode::Aggregate(spec) = &merge_mode {
        apply_federated_aggregate_having(&mut merged, spec, &pmap)
            .map_err(RouterError::InvalidArgument)?;
    }

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "hot_vertex_finalize",
        crate::current_instruction_counter().saturating_sub(hot_vertex_finalize_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let projection_advance_start = crate::current_instruction_counter();

    // No additional projection work here for non-bulk; token was built during the loop.
    let token = mutation_id.map(|mutation_id| MutationToken {
        mutation_id,
        shards: token_shards,
    });

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "projection_advance",
        crate::current_instruction_counter().saturating_sub(projection_advance_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let router_journal_update_start = crate::current_instruction_counter();

    if let Some(key) = mutation_key.as_ref()
        && let Some(row_count) = store.router_mutation_completed_row_count(key)
    {
        return Ok(attach_mutation_token(
            attach_mutation_phase(
                GqlQueryResult::row_count_only(row_count),
                store,
                caller,
                graph_id,
                client_mutation_key,
            ),
            token,
        ));
    }

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "router_journal_update",
        crate::current_instruction_counter().saturating_sub(router_journal_update_start),
    );

    #[cfg(feature = "batch-instr-log")]
    let result_materialize_start = crate::current_instruction_counter();

    let result = attach_mutation_token(
        attach_mutation_phase(
            GqlQueryResult::from_merged(&merged),
            store,
            caller,
            graph_id,
            client_mutation_key,
        ),
        token,
    );

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "result_materialize",
        crate::current_instruction_counter().saturating_sub(result_materialize_start),
    );

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "post_dispatch",
        crate::current_instruction_counter().saturating_sub(post_dispatch_start),
    );

    #[cfg(feature = "batch-instr-log")]
    log_router_dispatch_phase(
        "dispatch_total",
        crate::current_instruction_counter().saturating_sub(graph_request_build_start),
    );

    Ok(result)
}

/// Returns `true` when another dynamic Graph batch chunk fits the update message's
/// instruction ceiling: used + one chunk round (measured worst-case Router work) + the
/// `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM` finalization reserve must stay under 40B.
pub(crate) fn graph_batch_chunk_within_update_budget(used_instructions: u64) -> bool {
    !gleaph_instruction_budget::should_cutoff(
        gleaph_instruction_budget::MAX_UPDATE_CALL_INSTRUCTIONS,
        used_instructions,
        gleaph_instruction_budget::ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE,
        gleaph_instruction_budget::ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM,
        0,
    )
}

pub(crate) fn graph_batch_chunk_len_for_dispatches(
    dispatches: &[ShardDispatch],
    build_execute_args: &impl Fn(&ShardDispatch) -> gleaph_graph_kernel::plan_exec::ExecutePlanArgs,
    hint: Option<SizeHint>,
) -> Result<(usize, SizeHint), RouterError> {
    if dispatches.is_empty() {
        return Ok((0, SizeHint::new(0)));
    }
    let best = adaptive_fitting_prefix(
        dispatches.len(),
        hint,
        SizingPolicy::inter_canister(),
        |count| {
            let candidate = gleaph_graph_kernel::plan_exec::ExecutePlanBatchArgs {
                operations: dispatches[..count].iter().map(build_execute_args).collect(),
                mode: gleaph_graph_kernel::plan_exec::ExecutePlanBatchMode::Dynamic,
            };
            Encode!(&candidate)
                .map(|encoded| encoded.len())
                .map_err(|error| error.to_string())
        },
    )
    .map_err(|error| match error {
        FitError::Measure(detail) => {
            RouterError::InvalidArgument(format!("dispatch batch encode probe failed: {detail}"))
        }
        FitError::NoEntryFits {
            encoded_bytes,
            hard_limit_bytes,
        } => RouterError::InvalidArgument(format!(
            "single Graph batch operation is {encoded_bytes} bytes, above the safe limit of {hard_limit_bytes}"
        )),
    })?
    .ok_or_else(|| RouterError::InvalidArgument("empty Graph batch dispatch".into()))?;
    let instr_limited = gleaph_instruction_budget::max_operation_count(
        gleaph_graph_kernel::GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION,
        gleaph_graph_kernel::MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
    )
    .max(1);
    let chunk_len = best.entry_count.min(instr_limited);
    Ok((chunk_len, SizeHint::new(chunk_len)))
}

async fn dispatch_plan_blob_with_index_and_batch<I: IndexLookup + ?Sized>(
    graph_id: GraphId,
    plan_blob: &[u8],
    plans: &[PhysicalPlan],
    pmap: &BTreeMap<String, gleaph_gql::Value>,
    params: &[u8],
    mode: GqlExecutionMode,
    client_mutation_key: Option<&str>,
    store: &RouterStore,
    shards: Vec<ShardRegistryEntry>,
    index: &I,
    caller: Principal,
    stats: &RouterGraphStats,
    resolved_search: Option<BTreeMap<ShardId, gleaph_graph_kernel::plan_exec::ResolvedSearchWire>>,
    preflight: Option<&PreflightContext>,
) -> Result<GqlQueryResult, RouterError> {
    let prepared = prepare_mutation_for_batch(
        graph_id,
        plan_blob,
        plans,
        pmap,
        params,
        mode,
        client_mutation_key,
        store,
        shards,
        index,
        caller,
        stats,
        resolved_search,
        preflight,
        None,
        true,
    )
    .await?;
    match prepared {
        PrepareOutcome::Early(result) => Ok(result),
        PrepareOutcome::Prepared(prepared) => {
            execute_prepared_mutation(
                *prepared,
                store,
                caller,
                graph_id,
                client_mutation_key,
                preflight,
            )
            .await
        }
    }
}

/// Attach an ADR 0029 Phase 2 mutation token when one was issued for this dispatch.
pub(crate) fn attach_mutation_token(
    result: GqlQueryResult,
    token: Option<MutationToken>,
) -> GqlQueryResult {
    match token {
        Some(token) => result.with_token(token),
        None => result,
    }
}

fn release_routing_if_owner(
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_mutation_key: Option<&str>,
    mutation_reservation: Option<crate::facade::store::ClientMutationReservation>,
) -> Result<(), RouterError> {
    if let (Some(key), Some(reservation)) = (client_mutation_key, mutation_reservation)
        && reservation.routing_owner
    {
        store.abandon_router_mutation_routing_reservation(&mutation_key_for(
            caller, graph_id, key,
        ))?;
    }
    Ok(())
}

/// Confirm step of the cross-shard uniqueness TCC (ADR 0030 slice 5a).
///
/// Reads the replicated `Acquire` proof from each target the canonical write committed on, then for
/// every claim that has durable evidence transitions its reservation `Reserved → Committed` and acks
/// (unpins) the consumed effect. Best-effort by contract: the canonical write is already durable and
/// cannot be rolled back, so a failed read/ack leaves the reservation `Reserved` for the slice-6
/// recovery reconciler instead of failing the mutation that already succeeded.
async fn confirm_unique_reservations(
    store: &RouterStore,
    graph_id: GraphId,
    mutation_id: MutationId,
    claims: &[UniqueClaimDispatch],
    proof_targets: &[Principal],
) {
    let claim_ids: Vec<ClaimId> = claims
        .iter()
        .map(|claim| ClaimId::new(mutation_id, claim.claim_ordinal))
        .collect();
    for target in proof_targets {
        let proofs = match read_unique_effect_proof(*target, claim_ids.clone()).await {
            Ok(proofs) => proofs,
            // Leave reservations `Reserved`; slice-6 recovery reconciles them.
            Err(_) => continue,
        };
        let confirmed = confirm_proofs_collect_acks(store, graph_id, mutation_id, claims, proofs);
        if confirmed.is_empty() {
            continue;
        }
        let acked_effects: Vec<EffectId> = confirmed.iter().map(|(effect, _)| *effect).collect();
        // Clear `pending_acquire_ack` **only** after the ack succeeds (the effect is durably
        // unpinned). On a failed ack the records stay `Committed` with a pending ack, so slice-6
        // recovery re-discovers and re-acks them; clearing first would strand the pinned effect.
        if ack_unique_effects(*target, acked_effects).await.is_ok() {
            for (_, claim) in &confirmed {
                store.clear_unique_acquire_ack(
                    graph_id,
                    claim.constraint_id,
                    &claim.encoded_value,
                    ClaimId::new(mutation_id, claim.claim_ordinal),
                );
            }
        }
    }
}

/// Decision core of [`confirm_unique_reservations`], factored out from the inter-canister I/O so it
/// is unit-testable: given the proofs one target returned, confirm each backed claim and return, for
/// every claim that owns its commit, the `(effect_id, claim)` pair the caller acks then clears. The
/// safety contracts live here:
/// - **full `ClaimId` match**: the proof and the claim must agree on `(mutation_id, claim_ordinal)`,
///   so a stale/foreign proof can never confirm or ack a claim it does not own;
/// - **ack iff this claim owns the commit**: a pair is returned whenever `confirm_unique_claim`
///   returns `true`, i.e. the value is committed *by this claim* — either a fresh `Reserved →
///   Committed` move or an idempotent re-confirm of an already-`Committed` claim. The idempotent case
///   is intentional: a Confirm replayed after a previous ack failed must re-ack so the pinned effect
///   is eventually unpinned. A `false` (missing/`Reclaiming`/terminal-by-another-claim/mismatched)
///   must not ack, or it would unpin the sole durable commit evidence and make slice-6 recovery
///   misread the value as uncommitted.
///
/// Confirm stamps `pending_acquire_ack = effect_id` atomically with `→ Committed`; the caller clears
/// it only after the returned effects are acked, so the ack is crash-safe (recovery re-acks any that
/// were committed but not yet unpinned).
fn confirm_proofs_collect_acks<'a>(
    store: &RouterStore,
    graph_id: GraphId,
    mutation_id: MutationId,
    claims: &'a [UniqueClaimDispatch],
    proofs: Vec<gleaph_graph_kernel::federation::UniqueAcquireProof>,
) -> Vec<(EffectId, &'a UniqueClaimDispatch)> {
    let mut acked: Vec<(EffectId, &'a UniqueClaimDispatch)> = Vec::new();
    for proof in proofs {
        let Some(evidence) = proof.acquire else {
            continue;
        };
        let Some(claim) = claims
            .iter()
            .find(|claim| proof.claim_id == ClaimId::new(mutation_id, claim.claim_ordinal))
        else {
            continue;
        };
        // Ack on both committed outcomes (the idempotent re-ack retries a previously failed ack);
        // `NotApplicable` (missing/`Reclaiming`/foreign) must not ack. The non-terminal count is
        // decremented only on `FreshlyCommitted` — that reservation just left the non-terminal set,
        // so it no longer pins the owning record (slice-6 reverse index). An idempotent re-confirm
        // was already decremented on its first `FreshlyCommitted`, so it must not double-decrement.
        match store.confirm_unique_claim(
            graph_id,
            mutation_id,
            claim,
            evidence.owner_element_id,
            evidence.effect_id,
        ) {
            ConfirmOutcome::FreshlyCommitted => {
                store.release_unique_reservation_slot(mutation_id);
                acked.push((evidence.effect_id, claim));
            }
            ConfirmOutcome::AlreadyCommitted => {
                acked.push((evidence.effect_id, claim));
            }
            ConfirmOutcome::NotApplicable => {}
        }
    }
    acked
}

/// Page size for the Router's paginated `Release` reconciliation (ADR 0030 slice 5b). The shard
/// clamps the request to its own hard maximum, so this is an upper bound on effects pulled per call.
const UNIQUE_RELEASE_RECONCILE_PAGE: u32 = 256;

/// Release step of the cross-shard uniqueness TCC (ADR 0030 slice 5b).
///
/// Pages through each target's pinned `Release` effects for this mutation — an arbitrary-cardinality
/// DELETE/REMOVE can free unbounded values, so the effects are pulled by an `effect_ordinal` cursor
/// rather than in one response. Each page removes the matching `Committed` reservations (by
/// `owner_element_id`) and acks the consumed effects before advancing. Best-effort by contract: the
/// canonical delete is already durable, so a failed read/ack — or a `Release` held under the
/// Release-before-Acquire rule — leaves the effect pinned for the slice-6 recovery reconciler
/// instead of failing the mutation that already succeeded. The cursor advances past held effects so
/// reconciliation terminates; recovery revisits the still-pinned ones.
async fn reconcile_unique_releases(
    store: &RouterStore,
    graph_id: GraphId,
    mutation_id: MutationId,
    release_targets: &[Principal],
) {
    for target in release_targets {
        let mut cursor: Option<u32> = None;
        loop {
            let page = match read_unique_release_effects(
                *target,
                mutation_id,
                cursor,
                UNIQUE_RELEASE_RECONCILE_PAGE,
            )
            .await
            {
                Ok(page) => page,
                // Leave releases pinned; slice-6 recovery reconciles them.
                Err(_) => break,
            };
            // An empty page is the only end-of-stream signal: the shard clamps `limit` to its own
            // hard cap, so a short page does **not** imply the last page (a rolling upgrade or a
            // smaller shard cap would otherwise strand the releases past the first short page).
            let Some(last_ordinal) = page.last().map(|r| r.effect_id.effect_ordinal) else {
                break;
            };
            let acked_effects = reconcile_releases_collect_acks(store, graph_id, page);
            if !acked_effects.is_empty() {
                let _ = ack_unique_effects(*target, acked_effects).await;
            }
            // Advance past every effect observed (including held ones), so the loop terminates.
            cursor = Some(last_ordinal);
        }
    }
}

/// Decision core of [`reconcile_unique_releases`], factored out from the inter-canister I/O so it is
/// unit-testable: given the `Release` effects one target returned, apply each to the reservation
/// table and return the effect ids that may be acked. The safety contract lives here: an effect is
/// acked **only** when [`RouterStore::release_unique_effect`] reports the value durably free for this
/// owner (reservation removed, already gone, or a stale release a different element took over). A
/// **held** release (the value is still `Reserved`/`Reclaiming` or its owner is undetermined) is not
/// acked, so it stays pinned until slice-6 recovery reconciles the `Acquire` first — preventing the
/// Release-before-Acquire leak where a pending `Acquire` re-creates an already-deleted reservation.
fn reconcile_releases_collect_acks(
    store: &RouterStore,
    graph_id: GraphId,
    effects: Vec<gleaph_graph_kernel::federation::UniqueEffectReceipt>,
) -> Vec<EffectId> {
    let mut acked_effects: Vec<EffectId> = Vec::new();
    for effect in effects {
        if store.release_unique_effect(
            graph_id,
            effect.constraint_id,
            &effect.encoded_value,
            &effect.owner_element_id,
        ) {
            acked_effects.push(effect.effect_id);
        }
    }
    acked_effects
}

fn request_fingerprint(plan_blob: &[u8], params: &[u8], mode: GqlExecutionMode) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 + plan_blob.len() + 8 + params.len());
    out.push(match mode {
        GqlExecutionMode::Query => 0,
        GqlExecutionMode::Update => 1,
    });
    out.extend_from_slice(&(plan_blob.len() as u64).to_le_bytes());
    out.extend_from_slice(plan_blob);
    out.extend_from_slice(&(params.len() as u64).to_le_bytes());
    out.extend_from_slice(params);
    out
}

/// Version-tagged, length-delimited fingerprint for one scalar plan execution.
///
/// The plan mode, bytes, and parameter bytes are the identity for the internal GQL mutation
/// replay record. Ordered atomic-insert and durable bulk-load requests use their family-owned
/// fingerprints instead.
const LABEL_STATS_PROJECTION_BATCH_LIMIT: u32 = 1_000;

pub(crate) async fn advance_label_stats_projection_through(
    store: &RouterStore,
    graph_id: GraphId,
    graph_canister: Principal,
    shard_id: ShardId,
    target_seq: Option<ShardEventSeq>,
) -> Result<(), RouterError> {
    let Some(target) = target_seq else {
        return Ok(());
    };
    loop {
        let cursor = store.label_stats_projection_cursor(graph_id, shard_id);
        if cursor >= target {
            return Ok(());
        }
        let result = store
            .advance_label_stats_projection(
                graph_id,
                graph_canister,
                shard_id,
                LABEL_STATS_PROJECTION_BATCH_LIMIT,
                list_pending_label_stats_deltas,
                ack_label_stats_deltas_through,
            )
            .await?;
        if result.deltas_applied == 0 {
            return Err(RouterError::InvalidArgument(format!(
                "label stats projection lag for shard {shard_id}: cursor {cursor}, need {target}"
            )));
        }
    }
}

async fn reconcile_router_mutation_projection(
    store: &RouterStore,
    key: &ClientMutationKey,
    preflight: Option<&PreflightContext>,
) -> Result<(), RouterError> {
    let Some(record) = store.router_mutation_record(key) else {
        return Ok(());
    };
    for shard in record
        .shards()
        .iter()
        .filter(|shard| shard.completed() && !shard.projection_advanced())
    {
        let Some(entry) = recover_mutation_outcome(
            store,
            key.graph_id,
            shard.graph_canister(),
            shard.shard_id(),
            record.as_v1().mutation_id,
            preflight,
        )
        .await?
        else {
            return Err(RouterError::InvalidArgument(format!(
                "mutation {} completed on router shard {} but graph journal is unavailable",
                record.as_v1().mutation_id,
                shard.shard_id()
            )));
        };
        if !matches!(entry.state(), MutationJournalState::Completed) {
            return Err(RouterError::InvalidArgument(format!(
                "mutation {} completed on router shard {} but graph journal is not completed",
                record.as_v1().mutation_id,
                shard.shard_id()
            )));
        }
        store.record_router_mutation_shard_projection_advanced(key, shard.shard_id())?;
    }
    Ok(())
}

async fn fetch_journal_entry(
    preflight: Option<&PreflightContext>,
    graph_canister: Principal,
    mutation_id: MutationId,
    shard_id: ShardId,
) -> Result<Option<GraphMutationJournalEntryWire>, RouterError> {
    match preflight {
        Some(ctx) => {
            let entries = ctx
                .resolve_journal_entries(&[(graph_canister, mutation_id)])
                .await?;
            entries.into_iter().next().ok_or_else(|| {
                RouterError::InvalidArgument(format!(
                    "graph shard {shard_id} did not persist mutation journal entry for mutation {mutation_id}"
                ))
            })
        }
        None => get_mutation_journal_entry(graph_canister, mutation_id)
            .await
            .map_err(RouterError::InvalidArgument),
    }
}

async fn advance_mutation_label_stats_projection(
    store: &RouterStore,
    graph_id: GraphId,
    graph_canister: Principal,
    shard_id: ShardId,
    mutation_id: MutationId,
    preflight: Option<&PreflightContext>,
) -> Result<GraphMutationJournalEntryWire, RouterError> {
    let entry = fetch_journal_entry(preflight, graph_canister, mutation_id, shard_id).await?;
    let Some(entry) = entry else {
        return Err(RouterError::InvalidArgument(format!(
            "graph shard {shard_id} did not persist mutation journal entry for mutation {mutation_id}"
        )));
    };
    if !matches!(entry.state(), MutationJournalState::Completed) {
        return Err(RouterError::InvalidArgument(format!(
            "graph shard {shard_id} mutation {mutation_id} did not complete"
        )));
    }
    advance_label_stats_projection_through(
        store,
        graph_id,
        graph_canister,
        shard_id,
        entry.emitted_delta_last_seq(),
    )
    .await?;
    Ok(entry)
}

async fn recover_mutation_outcome(
    store: &RouterStore,
    graph_id: GraphId,
    graph_canister: Principal,
    shard_id: ShardId,
    mutation_id: MutationId,
    preflight: Option<&PreflightContext>,
) -> Result<Option<GraphMutationJournalEntryWire>, RouterError> {
    let Some(entry) = fetch_journal_entry(preflight, graph_canister, mutation_id, shard_id).await?
    else {
        return Ok(None);
    };
    if !matches!(entry.state(), MutationJournalState::Completed) {
        return Ok(None);
    }
    advance_label_stats_projection_through(
        store,
        graph_id,
        graph_canister,
        shard_id,
        entry.emitted_delta_last_seq(),
    )
    .await?;
    Ok(Some(entry))
}

/// ADR 0029 Phase 4: drive one non-terminal saga toward `Completed` using only safe,
/// idempotent projection/index convergence — the recovery driver never re-executes
/// canonical DML.
///
/// For each unfinished shard: if the graph mutation journal shows the canonical write
/// committed, advance that shard's label-stats projection and record it
/// completed+projected; once every shard is projected the record finalizes (terminal). For a
/// `CanonicalPending` record, the driver may adopt an exact completed Graph journal receipt but
/// records a diagnostic and stays pending for absent or non-exact evidence. It never re-dispatches
/// canonical DML because that is the one operation that risks double-apply.
///
/// Idempotent and bounded: safe to call concurrently with a client retry (both paths use
/// cursor-guarded projection advancement and idempotent record mutators).
#[cfg(target_family = "wasm")]
async fn recover_ordered_edge_batch_record(
    store: &RouterStore,
    key: &crate::facade::stable::label_stats::ClientMutationKey,
    record: &crate::facade::stable::label_stats::RouterMutationRecord,
) -> Result<(), RouterError> {
    use crate::facade::stable::label_stats::{
        OrderedEdgeBatchTargetProgressV1, RouterMutationPayloadV1,
    };
    use gleaph_graph_kernel::plan_exec::{
        OrderedMutationRetirementArgs, OrderedMutationRetirementArgsV1,
    };

    let mut replay = match record.payload() {
        RouterMutationPayloadV1::OrderedEdgeBatch(replay) => replay.clone(),
        _ => return Ok(()),
    };
    if matches!(
        replay.target.progress,
        OrderedEdgeBatchTargetProgressV1::CanonicalPending
    ) {
        reconcile_ordered_canonical_pending(
            store,
            key,
            record,
            CanonicalPendingReconciliationTrigger::BackgroundRecovery,
            None,
        )
        .await?;
        let current = store
            .router_mutation_record(key)
            .ok_or_else(|| RouterError::Internal("ordered recovery record disappeared".into()))?;
        replay = match current.payload() {
            RouterMutationPayloadV1::OrderedEdgeBatch(replay) => replay.clone(),
            _ => return Ok(()),
        };
        if matches!(
            replay.target.progress,
            OrderedEdgeBatchTargetProgressV1::CanonicalPending
        ) {
            return Ok(());
        }
    }
    let target = &replay.target;
    let graph = target.request.target_graph_canister;
    let shard_id = target.request.target_shard_id;
    let mutation_id = record.as_v1().mutation_id;
    let fingerprint = target.graph_request_fingerprint;
    let receipt = match &target.progress {
        OrderedEdgeBatchTargetProgressV1::CanonicalPending => return Ok(()),
        OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(receipt)
        | OrderedEdgeBatchTargetProgressV1::ProjectionPending(receipt)
        | OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(receipt)
        | OrderedEdgeBatchTargetProgressV1::RetirementPending(receipt) => receipt.clone(),
    };

    if matches!(
        target.progress,
        OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(_)
    ) {
        store.record_ordered_edge_batch_projection_pending(key, mutation_id, fingerprint)?;
    }
    if matches!(
        target.progress,
        OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(_)
            | OrderedEdgeBatchTargetProgressV1::ProjectionPending(_)
    ) {
        advance_label_stats_projection_through(
            store,
            key.graph_id,
            graph,
            shard_id,
            receipt.emitted_delta_last_seq,
        )
        .await?;
        store.record_ordered_edge_batch_projection_advanced(
            key,
            mutation_id,
            fingerprint,
            gleaph_graph_kernel::plan_exec::MutationTokenShard {
                shard_id,
                label_stats_seq: receipt.emitted_delta_last_seq,
            },
        )?;
    }

    let current = store
        .router_mutation_record(key)
        .ok_or_else(|| RouterError::Internal("ordered recovery record disappeared".into()))?;
    if matches!(
        current.payload(),
        RouterMutationPayloadV1::OrderedEdgeBatch(replay)
            if matches!(
                replay.target.progress,
                OrderedEdgeBatchTargetProgressV1::ProjectionAdvanced(_)
            )
    ) {
        store.record_ordered_edge_batch_retirement_pending(key, mutation_id, fingerprint)?;
    }

    let ack = retire_ordered_mutation_on_graph(
        graph,
        OrderedMutationRetirementArgs::V1(OrderedMutationRetirementArgsV1 {
            mutation_id,
            graph_request_fingerprint: fingerprint,
        }),
    )
    .await
    .map_err(RouterError::Internal)?;
    let gleaph_graph_kernel::plan_exec::OrderedMutationRetirementAck::V1(ack) = ack;
    if ack.mutation_id != mutation_id
        || ack.graph_request_fingerprint != fingerprint
        || ack.receipt != receipt
    {
        return Err(RouterError::InvalidArgument(
            "ordered retirement acknowledgement does not match durable target".into(),
        ));
    }
    store.record_ordered_edge_batch_retired(key, mutation_id, fingerprint, ack.receipt)
}

#[cfg(target_family = "wasm")]
/// Projection/retirement recovery for a durable ordered mixed batch. The mixed Graph receipt is
/// already sufficient to resume after a lost Router callback. Canonical-pending recovery adopts
/// exact completed journal evidence but never redispatches canonical DML from this driver.
async fn recover_ordered_mixed_batch_record(
    store: &RouterStore,
    key: &crate::facade::stable::label_stats::ClientMutationKey,
    record: &crate::facade::stable::label_stats::RouterMutationRecord,
) -> Result<(), RouterError> {
    use crate::facade::stable::label_stats::{
        OrderedMixedBatchTargetProgressV1, RouterMutationPayloadV1,
    };
    use gleaph_graph_kernel::plan_exec::{
        OrderedMutationRetirementArgs, OrderedMutationRetirementArgsV1,
    };

    let mut replay = match record.payload() {
        RouterMutationPayloadV1::OrderedMixedBatch(replay) => replay.clone(),
        _ => return Ok(()),
    };
    if matches!(
        replay.target.progress,
        OrderedMixedBatchTargetProgressV1::CanonicalPending
    ) {
        reconcile_ordered_canonical_pending(
            store,
            key,
            record,
            CanonicalPendingReconciliationTrigger::BackgroundRecovery,
            None,
        )
        .await?;
        let current = store
            .router_mutation_record(key)
            .ok_or_else(|| RouterError::Internal("ordered recovery record disappeared".into()))?;
        replay = match current.payload() {
            RouterMutationPayloadV1::OrderedMixedBatch(replay) => replay.clone(),
            _ => return Ok(()),
        };
        if matches!(
            replay.target.progress,
            OrderedMixedBatchTargetProgressV1::CanonicalPending
        ) {
            return Ok(());
        }
    }
    let target = &replay.target;
    let graph = target.request.target_graph_canister;
    let shard_id = target.request.target_shard_id;
    let mutation_id = record.as_v1().mutation_id;
    let fingerprint = target.graph_request_fingerprint;
    let receipt = match &target.progress {
        OrderedMixedBatchTargetProgressV1::CanonicalPending => return Ok(()),
        OrderedMixedBatchTargetProgressV1::CanonicalCommitted(receipt)
        | OrderedMixedBatchTargetProgressV1::ProjectionPending(receipt)
        | OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(receipt)
        | OrderedMixedBatchTargetProgressV1::RetirementPending(receipt) => receipt.clone(),
    };

    if matches!(
        target.progress,
        OrderedMixedBatchTargetProgressV1::CanonicalCommitted(_)
    ) {
        store.record_ordered_mixed_batch_projection_pending(key, mutation_id, fingerprint)?;
    }
    if matches!(
        target.progress,
        OrderedMixedBatchTargetProgressV1::CanonicalCommitted(_)
            | OrderedMixedBatchTargetProgressV1::ProjectionPending(_)
    ) {
        advance_label_stats_projection_through(
            store,
            key.graph_id,
            graph,
            shard_id,
            receipt.emitted_delta_last_seq,
        )
        .await?;
        store.record_ordered_mixed_batch_projection_advanced(
            key,
            mutation_id,
            fingerprint,
            MutationTokenShard {
                shard_id,
                label_stats_seq: receipt.emitted_delta_last_seq,
            },
        )?;
    }

    let current = store
        .router_mutation_record(key)
        .ok_or_else(|| RouterError::Internal("ordered mixed recovery record disappeared".into()))?;
    if matches!(
        current.payload(),
        RouterMutationPayloadV1::OrderedMixedBatch(replay)
            if matches!(
                replay.target.progress,
                OrderedMixedBatchTargetProgressV1::ProjectionAdvanced(_)
            )
    ) {
        store.record_ordered_mixed_batch_retirement_pending(key, mutation_id, fingerprint)?;
    }

    let ack = retire_ordered_mixed_mutation_on_graph(
        graph,
        OrderedMutationRetirementArgs::V1(OrderedMutationRetirementArgsV1 {
            mutation_id,
            graph_request_fingerprint: fingerprint,
        }),
    )
    .await
    .map_err(RouterError::Internal)?;
    let gleaph_graph_kernel::plan_exec::OrderedMixedMutationRetirementAck::V1(ack) = ack;
    if ack.mutation_id != mutation_id
        || ack.graph_request_fingerprint != fingerprint
        || ack.receipt != receipt
    {
        return Err(RouterError::InvalidArgument(
            "ordered mixed retirement acknowledgement does not match durable target".into(),
        ));
    }
    store.record_ordered_mixed_batch_retired(key, mutation_id, fingerprint, ack.receipt)
}

#[cfg(target_family = "wasm")]
/// Projection/retirement recovery for a durable ordered vertex batch. Canonical-pending recovery
/// adopts exact completed journal evidence without canonical redispatch; once Graph has committed,
/// the stored vertex receipt resumes projection and fingerprint-bound retirement without
/// reconstructing public input.
async fn recover_ordered_vertex_batch_record(
    store: &RouterStore,
    key: &crate::facade::stable::label_stats::ClientMutationKey,
    record: &crate::facade::stable::label_stats::RouterMutationRecord,
) -> Result<(), RouterError> {
    use crate::facade::stable::label_stats::{
        OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1,
    };
    use gleaph_graph_kernel::plan_exec::{
        OrderedVertexMutationRetirementArgs, OrderedVertexMutationRetirementArgsV1,
    };

    let mut replay = match record.payload() {
        RouterMutationPayloadV1::OrderedVertexBatch(replay) => replay.clone(),
        _ => return Ok(()),
    };
    if matches!(
        replay.target.progress,
        OrderedVertexBatchTargetProgressV1::CanonicalPending
    ) {
        reconcile_ordered_canonical_pending(
            store,
            key,
            record,
            CanonicalPendingReconciliationTrigger::BackgroundRecovery,
            None,
        )
        .await?;
        let current = store
            .router_mutation_record(key)
            .ok_or_else(|| RouterError::Internal("ordered recovery record disappeared".into()))?;
        replay = match current.payload() {
            RouterMutationPayloadV1::OrderedVertexBatch(replay) => replay.clone(),
            _ => return Ok(()),
        };
        if matches!(
            replay.target.progress,
            OrderedVertexBatchTargetProgressV1::CanonicalPending
        ) {
            return Ok(());
        }
    }
    let target = &replay.target;
    let graph = target.request.target_graph_canister;
    let shard_id = target.request.target_shard_id;
    let mutation_id = record.as_v1().mutation_id;
    let fingerprint = target.graph_request_fingerprint;
    let receipt = match &target.progress {
        OrderedVertexBatchTargetProgressV1::CanonicalPending => return Ok(()),
        OrderedVertexBatchTargetProgressV1::CanonicalCommitted(receipt)
        | OrderedVertexBatchTargetProgressV1::ProjectionPending(receipt)
        | OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(receipt)
        | OrderedVertexBatchTargetProgressV1::RetirementPending(receipt) => receipt.clone(),
    };

    if matches!(
        target.progress,
        OrderedVertexBatchTargetProgressV1::CanonicalCommitted(_)
    ) {
        store.record_ordered_vertex_batch_projection_pending(key, mutation_id, fingerprint)?;
    }
    if matches!(
        target.progress,
        OrderedVertexBatchTargetProgressV1::CanonicalCommitted(_)
            | OrderedVertexBatchTargetProgressV1::ProjectionPending(_)
    ) {
        advance_label_stats_projection_through(
            store,
            key.graph_id,
            graph,
            shard_id,
            receipt.emitted_delta_last_seq,
        )
        .await?;
        store.record_ordered_vertex_batch_projection_advanced(
            key,
            mutation_id,
            fingerprint,
            MutationTokenShard {
                shard_id,
                label_stats_seq: receipt.emitted_delta_last_seq,
            },
        )?;
    }

    let current = store.router_mutation_record(key).ok_or_else(|| {
        RouterError::Internal("ordered vertex recovery record disappeared".into())
    })?;
    if matches!(
        current.payload(),
        RouterMutationPayloadV1::OrderedVertexBatch(replay)
            if matches!(
                replay.target.progress,
                OrderedVertexBatchTargetProgressV1::ProjectionAdvanced(_)
            )
    ) {
        store.record_ordered_vertex_batch_retirement_pending(key, mutation_id, fingerprint)?;
    }

    let ack = retire_ordered_vertex_mutation_on_graph(
        graph,
        OrderedVertexMutationRetirementArgs::V1(OrderedVertexMutationRetirementArgsV1 {
            mutation_id,
            graph_request_fingerprint: fingerprint,
        }),
    )
    .await
    .map_err(RouterError::Internal)?;
    let gleaph_graph_kernel::plan_exec::OrderedVertexMutationRetirementAck::V1(ack) = ack;
    if ack.mutation_id != mutation_id
        || ack.graph_request_fingerprint != fingerprint
        || ack.receipt != receipt
    {
        return Err(RouterError::InvalidArgument(
            "ordered vertex retirement acknowledgement does not match durable target".into(),
        ));
    }
    store.record_ordered_vertex_batch_retired(key, mutation_id, fingerprint, ack.receipt)
}

#[cfg(target_family = "wasm")]
pub(crate) async fn recover_mutation_record(
    store: &RouterStore,
    key: &ClientMutationKey,
) -> Result<(), RouterError> {
    let Some(record) = store.router_mutation_record(key) else {
        return Ok(());
    };
    if record.is_terminal() {
        return Ok(());
    }
    let mutation_id = record.as_v1().mutation_id;

    if matches!(
        record.payload(),
        crate::facade::stable::label_stats::RouterMutationPayloadV1::OrderedEdgeBatch(_)
    ) {
        return match recover_ordered_edge_batch_record(store, key, &record).await {
            Ok(()) => Ok(()),
            Err(error) => {
                store.record_router_mutation_last_error(key, error.to_string())?;
                Ok(())
            }
        };
    }

    if matches!(
        record.payload(),
        crate::facade::stable::label_stats::RouterMutationPayloadV1::OrderedVertexBatch(_)
    ) {
        return match recover_ordered_vertex_batch_record(store, key, &record).await {
            Ok(()) => Ok(()),
            Err(error) => {
                store.record_router_mutation_last_error(key, error.to_string())?;
                Ok(())
            }
        };
    }

    if matches!(
        record.payload(),
        crate::facade::stable::label_stats::RouterMutationPayloadV1::OrderedMixedBatch(_)
    ) {
        return match recover_ordered_mixed_batch_record(store, key, &record).await {
            Ok(()) => Ok(()),
            Err(error) => {
                store.record_router_mutation_last_error(key, error.to_string())?;
                Ok(())
            }
        };
    }

    for shard in record.shards() {
        if shard.completed() && shard.projection_advanced() {
            continue;
        }
        match recover_mutation_outcome(
            store,
            key.graph_id,
            shard.graph_canister(),
            shard.shard_id(),
            mutation_id,
            None,
        )
        .await?
        {
            Some(entry) => {
                if !shard.completed() {
                    store.record_router_mutation_shard_completed(
                        key,
                        shard.shard_id(),
                        entry.row_count(),
                    )?;
                }
                store.record_router_mutation_shard_projection_advanced(key, shard.shard_id())?;
            }
            None => {
                store.record_router_mutation_last_error(
                    key,
                    format!(
                        "shard {} canonical write not yet committed; retry the idempotent \
                         mutation to resume",
                        shard.shard_id()
                    ),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CanonicalPendingReconciliationTrigger, PreflightContext, mutation_key_for};
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, HashMap};
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    use crate::federation::SeedHits;
    use crate::index_lookup::IndexLookup;
    use candid::{Decode, Principal};
    use gleaph_gql::Value;
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql::parser;
    use gleaph_gql_ic::{GqlWireRows, GqlWireValue};
    use gleaph_gql_planner::plan::ScanValue;
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_gql_planner::{NodeLabelRef, PhysicalPlan, PlanOp};
    use gleaph_graph_kernel::index::{
        EdgePostingHit, IndexLabelIntersectionRequest, LabelLookupPageResult, PhysicalIndexId,
        PostingHit, ValuePostingCount,
    };
    use gleaph_graph_kernel::plan_exec::{
        ExecutePlanResult, GqlExecutionMode, GqlQueryResult, GraphMutationJournalEntryWire,
        GraphMutationRequestIdentityV1, GraphMutationRetirementWireV1,
        GraphOrderedEdgeBatchReceiptV1, GraphOrderedEdgeBatchResult, GraphOrderedEdgeBatchResultV1,
        GraphOrderedMixedBatchReceiptV1, GraphOrderedMixedBatchResult,
        GraphOrderedMixedBatchResultV1, GraphOrderedVertexBatchReceiptV1,
        GraphOrderedVertexBatchResult, GraphOrderedVertexBatchResultV1, LabelStatsDelta,
        LabelStatsDeltaEventWire, MutationId, MutationJournalState, MutationToken,
        MutationTokenShard, OrderedBatchExecutionModeV1, OrderedEdgeBatchGraphArgs,
        OrderedEdgeBatchGraphArgsV1, OrderedEdgeBatchGraphItemV1, OrderedEdgeBatchGraphRequest,
        OrderedEdgeBatchGraphRequestV1, OrderedMixedBatchGraphArgs, OrderedMixedBatchGraphArgsV1,
        OrderedMixedBatchGraphRequest, OrderedMixedBatchGraphRequestV1,
        OrderedMixedGraphEdgeItemV1, OrderedMixedGraphEndpointV1, OrderedMixedGraphOperationV1,
        OrderedVertexBatchGraphArgs, OrderedVertexBatchGraphArgsV1, OrderedVertexBatchGraphItemV1,
        OrderedVertexBatchGraphRequest, OrderedVertexBatchGraphRequestV1, ReadMode,
        SeedBindingsWire, ordered_edge_batch_graph_request_fingerprint,
        ordered_mixed_batch_graph_request_fingerprint,
        ordered_vertex_batch_graph_request_fingerprint,
    };

    #[test]
    fn gql_result_payload_guard_rejects_oversized_rows_blob() {
        let result = GqlQueryResult {
            row_count: 0,
            rows_blob: Some(vec![
                0;
                gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
            ]),
            phase: None,
            token: None,
            truncated: None,
        };
        let err = super::ensure_gql_query_result_payload(&result, "test")
            .expect_err("oversized GQL result");
        assert!(
            err.to_string()
                .contains("test response exceeds the safe payload limit")
        );
    }

    #[test]
    fn vector_ddl_ingress_enforces_rbac_before_catalog_side_effects() {
        use gleaph_auth::AdminCaps;

        let store = store_with_one_shard();
        let graph_id = tenant_main_graph_id();
        let admin = Principal::from_slice(&[1; 29]);
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Document")
            .expect("intern vector label");
        let query = r#"CREATE VECTOR INDEX document_embedding_idx
            FOR (d:Document) ON d.embedding
            OPTIONS { dimensions: 384, metric: "cosine", encoding: "f32", algorithm: "ivf_flat" }"#;
        let next_before =
            crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get());

        let anonymous = Principal::anonymous();
        let anonymous_err = futures::executor::block_on(super::try_execute_vector_index_ddl(
            query,
            GqlExecutionMode::Update,
            "gql_mutate",
            false,
            || anonymous,
            |_| Ok(graph_id),
        ))
        .expect("vector DDL detected")
        .expect_err("anonymous caller must be rejected");
        assert!(matches!(anonymous_err, RouterError::Forbidden));

        let capless = Principal::self_authenticating([31; 32]);
        crate::facade::stable::ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
            auth.upsert_caps(capless, AdminCaps::MANAGE_CATALOG)
                .expect("capless auth record");
        });
        let capless_err = futures::executor::block_on(super::try_execute_vector_index_ddl(
            query,
            GqlExecutionMode::Update,
            "gql_mutate",
            false,
            || capless,
            |_| Ok(graph_id),
        ))
        .expect("vector DDL detected")
        .expect_err("caller without an index cap must be rejected");
        assert!(matches!(capless_err, RouterError::Forbidden));
        assert_eq!(
            crate::facade::stable::index_name_catalog::lookup_index_name_id(
                graph_id,
                "document_embedding_idx"
            ),
            None
        );
        assert_eq!(
            crate::facade::stable::embedding_name_catalog::lookup_embedding_name_id(
                graph_id,
                "embedding"
            ),
            None
        );
        assert!(
            crate::facade::stable::vector_index_catalog::list_vector_indexes(graph_id).is_empty()
        );
        assert_eq!(
            crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_before
        );

        let index_admin = Principal::self_authenticating([32; 32]);
        crate::facade::stable::ROUTER_AUTH_STATE.with_borrow_mut(|auth| {
            auth.upsert_caps(index_admin, AdminCaps::INDEX_CREATE)
                .expect("index-create auth record");
        });
        let result = futures::executor::block_on(super::try_execute_vector_index_ddl(
            query,
            GqlExecutionMode::Update,
            "gql_mutate",
            false,
            || index_admin,
            |_| Ok(graph_id),
        ))
        .expect("vector DDL detected")
        .expect("caller with INDEX_CREATE may create");
        assert_eq!(result.row_count, 0);
        let index_name_id = crate::facade::stable::index_name_catalog::lookup_index_name_id(
            graph_id,
            "document_embedding_idx",
        )
        .expect("logical name created");
        let def = crate::facade::stable::vector_index_catalog::get_vector_index_by_name_id(
            graph_id,
            index_name_id,
        )
        .expect("definition created");
        assert!(def.target.is_none());
    }

    use crate::facade::stable::graph_catalog::lookup_graph_id;
    use crate::facade::stable::label_stats::{
        ClientMutationKey, OrderedEdgeBatchTargetProgressV1, OrderedMixedBatchTargetProgressV1,
        OrderedVertexBatchTargetProgressV1, RouterMutationPayloadV1, RouterMutationRecord,
        RouterMutationRequestIdentityV1, RouterOrderedEdgeBatchTargetV1,
        RouterOrderedMixedBatchTargetV1, RouterOrderedVertexBatchTargetV1,
    };
    use crate::facade::store::RouterStore;
    use crate::federation::{
        collect_label_intersection_hits_for_shards, resolve_seed_routings_multi,
        routings_to_dispatches,
    };
    use crate::gql::{
        dispatch_plan_blob_with_index, enforce_multi_dml_bundle_gate, enforce_read_consistency,
        enforce_read_consistency_with_lookup, request_fingerprint,
    };
    use crate::init::RouterInitArgs;
    use crate::planner_stats::RouterGraphStats;
    use crate::seed::SeedAnchorSet;
    use crate::seed::{IndexAnchor, SeedProbe, seeds_for_local_shard};
    use crate::state::RouterError;
    use crate::types::{
        AdminRegisterShardArgs, AtomicInsertEdgeV1, AtomicInsertEndpointV1,
        AtomicInsertOperationV1, AtomicInsertRequest, AtomicInsertRequestV1, AtomicInsertVertexV1,
        GraphRegistryEntry, GraphStatus, ProvisioningState,
    };
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::ShardId;
    use std::collections::BTreeSet;

    fn graph_principal(byte: u8) -> Principal {
        Principal::self_authenticating([byte; 32])
    }

    /// ADR 0030 slice 10 fail-closed gate: a `ShardLocalGlobal` mutation may only proceed when the
    /// graph still has exactly one live shard whose `(shard_id, graph_canister)` identity matches the
    /// claim's recorded owning shard. Every other topology must be rejected (never FederatedTcc
    /// fallback), so this proves `local_claims_enforceable` returns the safe decision in each shape.
    #[test]
    fn local_claims_enforceable_only_when_sole_live_shard_matches_owner() {
        use crate::facade::stable::reservation_catalog::ProofShard;
        use crate::facade::store::uniqueness::LocalUniqueClaim;
        use crate::gql::local_claims_enforceable;
        use gleaph_graph_kernel::entry::ConstraintNameId;
        use gleaph_graph_kernel::federation::ShardRegistryEntry;
        use gleaph_graph_kernel::plan_exec::UniqueClaimDispatch;

        let graph_id = GraphId::from_raw(7);
        let owner_canister = graph_principal(1);
        let owner_shard = ShardId::new(0);

        let shard = |shard_id: ShardId, canister: Principal| ShardRegistryEntry {
            shard_id,
            graph_canister: canister,
            index_canister: Principal::anonymous(),
            graph_id,
            registered_at_ns: 0,
            index_attached: true,
            vector_canister: None,
            vector_index_attached: false,
        };
        let claim = |shard_id: ShardId, canister: Principal| LocalUniqueClaim {
            dispatch: UniqueClaimDispatch {
                claim_ordinal: 0,
                constraint_id: ConstraintNameId::from_raw(1),
                encoded_value: b"v".to_vec(),
            },
            owning_shard: ProofShard::new(shard_id, canister),
        };
        let claims = vec![claim(owner_shard, owner_canister)];

        // Sole live shard with the exact recorded identity: enforceable.
        assert!(local_claims_enforceable(
            &[shard(owner_shard, owner_canister)],
            &claims
        ));

        // No live shard: cannot prove uniqueness — reject.
        assert!(!local_claims_enforceable(&[], &claims));

        // A second shard appeared (scale-out): reject fail-closed.
        assert!(!local_claims_enforceable(
            &[
                shard(owner_shard, owner_canister),
                shard(ShardId::new(1), graph_principal(2)),
            ],
            &claims
        ));

        // Same shard_id but re-homed on a different canister: identity mismatch — reject.
        assert!(!local_claims_enforceable(
            &[shard(owner_shard, graph_principal(9))],
            &claims
        ));

        // Different shard_id on the (otherwise sole) live shard: identity mismatch — reject.
        assert!(!local_claims_enforceable(
            &[shard(ShardId::new(5), owner_canister)],
            &claims
        ));
    }

    fn register_test_graph(store: &RouterStore, admin: Principal, name: &str) {
        store
            .admin_register_graph(
                admin,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: Principal::management_canister(),
                    owner: admin,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
                name,
            )
            .expect("register graph");
    }

    fn tenant_main_graph_id() -> GraphId {
        lookup_graph_id("tenant.main").expect("tenant.main")
    }

    fn tenant_main_stats() -> RouterGraphStats {
        RouterGraphStats::from_catalog(
            tenant_main_graph_id(),
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        )
    }

    fn store_with_shards_spec(shards: &[(ShardId, u8)]) -> RouterStore {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        for (shard_id, graph_byte) in shards {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id: *shard_id,
                    graph_canister: graph_principal(*graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        store
    }

    fn store_with_shards() -> RouterStore {
        store_with_shards_spec(&[(ShardId::new(0), 1u8), (ShardId::new(1), 4)])
    }

    fn store_with_one_shard() -> RouterStore {
        store_with_shards_spec(&[(ShardId::new(0), 1u8)])
    }

    fn ordered_test_caller() -> Principal {
        Principal::from_slice(&[1; 29])
    }

    fn ordered_test_graph() -> Principal {
        graph_principal(1)
    }

    fn edge_target() -> RouterOrderedEdgeBatchTargetV1 {
        let request = OrderedEdgeBatchGraphRequestV1 {
            graph_id: tenant_main_graph_id(),
            target_shard_id: ShardId::new(0),
            target_graph_canister: ordered_test_graph(),
            resolved_labels: Default::default(),
            resolved_properties: Default::default(),
            items: vec![OrderedEdgeBatchGraphItemV1 {
                source_local_vertex_id: 10,
                target_local_vertex_id: 11,
                directed: true,
                catalog_edge_label_id: None,
                inline_property_bytes: Vec::new(),
                resolved_initial_edge_properties: Vec::new(),
            }],
        };
        let graph_request = OrderedEdgeBatchGraphRequest::V1(request.clone());
        let graph_request_fingerprint =
            ordered_edge_batch_graph_request_fingerprint(&graph_request)
                .expect("edge Graph request fingerprint");
        RouterOrderedEdgeBatchTargetV1 {
            graph_request_fingerprint,
            request,
            progress: OrderedEdgeBatchTargetProgressV1::CanonicalPending,
            projection_watermark: None,
        }
    }

    fn vertex_target() -> RouterOrderedVertexBatchTargetV1 {
        let request = OrderedVertexBatchGraphRequestV1 {
            graph_id: tenant_main_graph_id(),
            target_shard_id: ShardId::new(0),
            target_graph_canister: ordered_test_graph(),
            resolved_labels: Default::default(),
            resolved_properties: Default::default(),
            items: vec![OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: Vec::new(),
                resolved_initial_properties: Vec::new(),
            }],
        };
        let graph_request = OrderedVertexBatchGraphRequest::V1(request.clone());
        let graph_request_fingerprint =
            ordered_vertex_batch_graph_request_fingerprint(&graph_request)
                .expect("vertex Graph request fingerprint");
        RouterOrderedVertexBatchTargetV1 {
            graph_request_fingerprint,
            request,
            progress: OrderedVertexBatchTargetProgressV1::CanonicalPending,
            projection_watermark: None,
        }
    }

    fn mixed_target() -> RouterOrderedMixedBatchTargetV1 {
        let request = OrderedMixedBatchGraphRequestV1 {
            graph_id: tenant_main_graph_id(),
            target_shard_id: ShardId::new(0),
            target_graph_canister: ordered_test_graph(),
            resolved_labels: Default::default(),
            resolved_properties: Default::default(),
            operations: vec![
                OrderedMixedGraphOperationV1::Vertex(OrderedVertexBatchGraphItemV1 {
                    resolved_vertex_labels: Vec::new(),
                    resolved_initial_properties: Vec::new(),
                }),
                OrderedMixedGraphOperationV1::Edge(OrderedMixedGraphEdgeItemV1 {
                    source: OrderedMixedGraphEndpointV1::Existing(10),
                    target: OrderedMixedGraphEndpointV1::Existing(11),
                    directed: true,
                    catalog_edge_label_id: None,
                    inline_property_bytes: Vec::new(),
                    resolved_initial_edge_properties: Vec::new(),
                }),
            ],
        };
        let graph_request = OrderedMixedBatchGraphRequest::V1(request.clone());
        let graph_request_fingerprint =
            ordered_mixed_batch_graph_request_fingerprint(&graph_request)
                .expect("mixed Graph request fingerprint");
        RouterOrderedMixedBatchTargetV1 {
            graph_request_fingerprint,
            request,
            progress: OrderedMixedBatchTargetProgressV1::CanonicalPending,
            projection_watermark: None,
        }
    }

    fn edge_receipt() -> GraphOrderedEdgeBatchReceiptV1 {
        GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
        }
    }

    fn vertex_receipt() -> GraphOrderedVertexBatchReceiptV1 {
        GraphOrderedVertexBatchReceiptV1 {
            logical_vertex_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
            allocated_vertex_ids: vec![7],
        }
    }

    fn mixed_receipt() -> GraphOrderedMixedBatchReceiptV1 {
        GraphOrderedMixedBatchReceiptV1 {
            logical_operation_count: 2,
            logical_vertex_count: 1,
            logical_edge_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
            allocated_vertex_ids: vec![7],
        }
    }

    fn seed_edge_pending(
        store: &RouterStore,
        client_key: &str,
        public_fingerprint: [u8; 32],
        target: RouterOrderedEdgeBatchTargetV1,
    ) -> (ClientMutationKey, MutationId) {
        let caller = ordered_test_caller();
        let graph_id = tenant_main_graph_id();
        let key = ClientMutationKey::new(caller, graph_id, client_key.to_owned());
        let reservation = store
            .reserve_mutation_id_for_client_key(
                caller,
                graph_id,
                client_key,
                public_fingerprint.to_vec(),
            )
            .expect("edge mutation reservation");
        store
            .transition_to_ordered_edge_batch(
                &key,
                reservation.mutation_id,
                RouterMutationRequestIdentityV1::OrderedEdgeBatch {
                    public_fingerprint,
                    public_item_count: target.request.items.len() as u32,
                },
                Default::default(),
                Default::default(),
                target,
            )
            .expect("edge pending transition");
        (key, reservation.mutation_id)
    }

    fn seed_vertex_pending(
        store: &RouterStore,
        client_key: &str,
        public_fingerprint: [u8; 32],
        target: RouterOrderedVertexBatchTargetV1,
    ) -> (ClientMutationKey, MutationId) {
        let caller = ordered_test_caller();
        let graph_id = tenant_main_graph_id();
        let key = ClientMutationKey::new(caller, graph_id, client_key.to_owned());
        let reservation = store
            .reserve_mutation_id_for_client_key(
                caller,
                graph_id,
                client_key,
                public_fingerprint.to_vec(),
            )
            .expect("vertex mutation reservation");
        store
            .transition_to_ordered_vertex_batch(
                &key,
                reservation.mutation_id,
                RouterMutationRequestIdentityV1::OrderedVertexBatch {
                    public_fingerprint,
                    public_item_count: target.request.items.len() as u32,
                },
                Default::default(),
                Default::default(),
                target,
            )
            .expect("vertex pending transition");
        (key, reservation.mutation_id)
    }

    fn seed_mixed_pending(
        store: &RouterStore,
        client_key: &str,
        public_fingerprint: [u8; 32],
        target: RouterOrderedMixedBatchTargetV1,
    ) -> (ClientMutationKey, MutationId) {
        let caller = ordered_test_caller();
        let graph_id = tenant_main_graph_id();
        let key = ClientMutationKey::new(caller, graph_id, client_key.to_owned());
        let reservation = store
            .reserve_mutation_id_for_client_key(
                caller,
                graph_id,
                client_key,
                public_fingerprint.to_vec(),
            )
            .expect("mixed mutation reservation");
        store
            .transition_to_ordered_mixed_batch(
                &key,
                reservation.mutation_id,
                RouterMutationRequestIdentityV1::OrderedMixedBatch {
                    public_fingerprint,
                    public_operation_count: target.request.operations.len() as u32,
                    public_vertex_count: 1,
                    public_edge_count: 1,
                },
                Default::default(),
                Default::default(),
                target,
            )
            .expect("mixed pending transition");
        (key, reservation.mutation_id)
    }

    fn preflight_with_journal(
        graph: Principal,
        mutation_id: MutationId,
        entry: Option<GraphMutationJournalEntryWire>,
    ) -> PreflightContext {
        let mut journal_entries = HashMap::new();
        journal_entries.insert((graph, mutation_id), entry);
        PreflightContext {
            anchor_hits: Rc::new(RefCell::new(HashMap::new())),
            journal_entries: Rc::new(RefCell::new(journal_entries)),
            pending_min_mutation_id: Rc::new(RefCell::new(HashMap::new())),
            plan_cache: Rc::new(RefCell::new(HashMap::new())),
            resolved_labels: Rc::new(RefCell::new(HashMap::new())),
            resolved_properties: Rc::new(RefCell::new(HashMap::new())),
            graph_catalog: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn completed_journal(
        mutation_id: MutationId,
        identity: GraphMutationRequestIdentityV1,
        row_count: u64,
        allocated_vertex_ids: Option<Vec<u32>>,
    ) -> GraphMutationJournalEntryWire {
        let mut entry = GraphMutationJournalEntryWire::new(
            mutation_id,
            MutationJournalState::Completed,
            row_count,
            None,
            None,
            Vec::new(),
        );
        entry.set_request_identity(identity);
        entry.set_retirement(GraphMutationRetirementWireV1::Active);
        entry.set_allocated_vertex_ids(allocated_vertex_ids);
        entry.validate().expect("valid ordered journal fixture");
        entry
    }

    fn ordered_public_request(family: OrderedTestFamily, client_key: &str) -> AtomicInsertRequest {
        let existing_endpoint = || {
            AtomicInsertEndpointV1::Existing(vec![
                0;
                gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
            ])
        };
        let operations = match family {
            OrderedTestFamily::Edge => vec![AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                source: existing_endpoint(),
                target: existing_endpoint(),
                directed: true,
                edge_label_name: None,
                inline_property: None,
                initial_edge_properties: Vec::new(),
            })],
            OrderedTestFamily::Vertex => {
                vec![AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                    vertex_labels: Vec::new(),
                    initial_properties: Vec::new(),
                })]
            }
            OrderedTestFamily::Mixed => vec![
                AtomicInsertOperationV1::Vertex(AtomicInsertVertexV1 {
                    vertex_labels: Vec::new(),
                    initial_properties: Vec::new(),
                }),
                AtomicInsertOperationV1::Edge(AtomicInsertEdgeV1 {
                    source: existing_endpoint(),
                    target: existing_endpoint(),
                    directed: true,
                    edge_label_name: None,
                    inline_property: None,
                    initial_edge_properties: Vec::new(),
                }),
            ],
        };
        AtomicInsertRequest::V1(AtomicInsertRequestV1 {
            client_mutation_key: client_key.to_owned(),
            graph_name: Some("tenant.main".into()),
            operations,
        })
    }

    fn assert_pending_diagnostic(
        before: &RouterMutationRecord,
        after: &RouterMutationRecord,
        case: &str,
    ) {
        let mut expected = before.as_v1().clone();
        expected.last_error = after.as_v1().last_error.clone();
        assert_eq!(
            &expected,
            after.as_v1(),
            "{case}: only the bounded diagnostic may change"
        );
        assert_eq!(
            before.as_v1().payload,
            after.as_v1().payload,
            "{case}: rejected evidence must leave the immutable target and progress unchanged"
        );
        assert_eq!(
            before.as_v1().request_identity,
            after.as_v1().request_identity,
            "{case}: rejected evidence must leave public identity unchanged"
        );
        let diagnostic = after
            .as_v1()
            .last_error
            .as_deref()
            .expect("{case}: reconciliation must record a diagnostic");
        assert!(
            diagnostic.starts_with("ordered "),
            "{case}: unexpected reconciliation diagnostic: {diagnostic}"
        );
        assert!(
            diagnostic.len()
                <= crate::facade::stable::label_stats::MAX_MUTATION_RECOVERY_DIAGNOSTIC_BYTES,
            "{case}: diagnostic exceeded its bounded storage contract"
        );
        assert!(
            matches!(
                after.payload(),
                RouterMutationPayloadV1::OrderedEdgeBatch(replay)
                    if matches!(replay.target.progress, OrderedEdgeBatchTargetProgressV1::CanonicalPending)
            ) || matches!(
                after.payload(),
                RouterMutationPayloadV1::OrderedVertexBatch(replay)
                    if matches!(replay.target.progress, OrderedVertexBatchTargetProgressV1::CanonicalPending)
            ) || matches!(
                after.payload(),
                RouterMutationPayloadV1::OrderedMixedBatch(replay)
                    if matches!(replay.target.progress, OrderedMixedBatchTargetProgressV1::CanonicalPending)
            )
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum OrderedTestFamily {
        Edge,
        Vertex,
        Mixed,
    }

    const ORDERED_TEST_FAMILIES: [OrderedTestFamily; 3] = [
        OrderedTestFamily::Edge,
        OrderedTestFamily::Vertex,
        OrderedTestFamily::Mixed,
    ];

    fn stored_edge_dispatch_args(
        mutation_id: MutationId,
        target: &RouterOrderedEdgeBatchTargetV1,
    ) -> OrderedEdgeBatchGraphArgs {
        OrderedEdgeBatchGraphArgs::V1(OrderedEdgeBatchGraphArgsV1 {
            mutation_id,
            graph_request_fingerprint: target.graph_request_fingerprint,
            execution_mode: OrderedBatchExecutionModeV1::Atomic,
            indexed_property_catalog:
                crate::facade::stable::indexed_catalog::ordered_edge_batch_catalog(&target.request),
            request: OrderedEdgeBatchGraphRequest::V1(target.request.clone()),
        })
    }

    fn stored_vertex_dispatch_args(
        mutation_id: MutationId,
        target: &RouterOrderedVertexBatchTargetV1,
    ) -> OrderedVertexBatchGraphArgs {
        OrderedVertexBatchGraphArgs::V1(OrderedVertexBatchGraphArgsV1 {
            mutation_id,
            graph_request_fingerprint: target.graph_request_fingerprint,
            execution_mode: OrderedBatchExecutionModeV1::Atomic,
            indexed_property_catalog:
                crate::facade::stable::indexed_catalog::ordered_vertex_batch_catalog(
                    &target.request,
                ),
            request: OrderedVertexBatchGraphRequest::V1(target.request.clone()),
        })
    }

    fn stored_mixed_dispatch_args(
        mutation_id: MutationId,
        target: &RouterOrderedMixedBatchTargetV1,
    ) -> OrderedMixedBatchGraphArgs {
        OrderedMixedBatchGraphArgs::V1(OrderedMixedBatchGraphArgsV1 {
            mutation_id,
            graph_request_fingerprint: target.graph_request_fingerprint,
            execution_mode: OrderedBatchExecutionModeV1::Atomic,
            indexed_property_catalog:
                crate::facade::stable::indexed_catalog::ordered_mixed_batch_catalog(&target.request),
            request: OrderedMixedBatchGraphRequest::V1(target.request.clone()),
        })
    }

    #[test]
    fn canonical_pending_reconciliation_uses_stored_target_before_fresh_resolution() {
        super::reset_test_reconciliation_dispatches();
        super::set_ordered_test_caller(Some(ordered_test_caller()));

        let store = store_with_shards_spec(&[]);
        assert!(
            store
                .list_live_shards_for_graph_id(tenant_main_graph_id())
                .expect("current live routing")
                .is_empty(),
            "fresh routing must be observably unavailable in this stored-target fixture"
        );
        for family in ORDERED_TEST_FAMILIES {
            let client_key = format!("stored-target-first-{family:?}");
            let request = ordered_public_request(family, &client_key);
            let public_fingerprint = request
                .public_fingerprint()
                .expect("public request fingerprint");
            let (key, expected_payload) = match family {
                OrderedTestFamily::Edge => {
                    let mut target = edge_target();
                    target.request.target_shard_id = ShardId::new(77);
                    target.request.target_graph_canister = graph_principal(9);
                    target.graph_request_fingerprint =
                        ordered_edge_batch_graph_request_fingerprint(
                            &OrderedEdgeBatchGraphRequest::V1(target.request.clone()),
                        )
                        .expect("stored edge Graph request fingerprint");
                    let (key, _) =
                        seed_edge_pending(&store, &client_key, public_fingerprint, target);
                    let payload = store
                        .router_mutation_record(&key)
                        .expect("stored edge record")
                        .as_v1()
                        .payload
                        .clone();
                    (key, payload)
                }
                OrderedTestFamily::Vertex => {
                    let mut target = vertex_target();
                    target.request.target_shard_id = ShardId::new(77);
                    target.request.target_graph_canister = graph_principal(9);
                    target.graph_request_fingerprint =
                        ordered_vertex_batch_graph_request_fingerprint(
                            &OrderedVertexBatchGraphRequest::V1(target.request.clone()),
                        )
                        .expect("stored vertex Graph request fingerprint");
                    let (key, _) =
                        seed_vertex_pending(&store, &client_key, public_fingerprint, target);
                    let payload = store
                        .router_mutation_record(&key)
                        .expect("stored vertex record")
                        .as_v1()
                        .payload
                        .clone();
                    (key, payload)
                }
                OrderedTestFamily::Mixed => {
                    let mut target = mixed_target();
                    target.request.target_shard_id = ShardId::new(77);
                    target.request.target_graph_canister = graph_principal(9);
                    target.graph_request_fingerprint =
                        ordered_mixed_batch_graph_request_fingerprint(
                            &OrderedMixedBatchGraphRequest::V1(target.request.clone()),
                        )
                        .expect("stored mixed Graph request fingerprint");
                    let (key, _) =
                        seed_mixed_pending(&store, &client_key, public_fingerprint, target);
                    let payload = store
                        .router_mutation_record(&key)
                        .expect("stored mixed record")
                        .as_v1()
                        .payload
                        .clone();
                    (key, payload)
                }
            };

            let response = futures::executor::block_on(super::atomic_insert_public(request))
                .expect("same-key pending retry");
            let after = store
                .router_mutation_record(&key)
                .expect("stored pending record after retry");

            assert_eq!(super::test_reconciliation_dispatch_calls(), 0);
            assert!(response.receipt.is_none());
            assert_eq!(after.as_v1().payload, expected_payload);
            assert!(
                after
                    .as_v1()
                    .last_error
                    .as_deref()
                    .is_some_and(|error| error.contains("get_mutation_journal_entry unavailable")),
                "{family:?}: stored-target journal query error must remain diagnostic"
            );
        }

        for family in ORDERED_TEST_FAMILIES {
            let client_key = format!("fresh-admission-reject-{family:?}");
            let (store, request, expected_error) = match family {
                OrderedTestFamily::Edge => {
                    let store = store_with_one_shard();
                    let encoding_key = store
                        .graph_element_id_encoding_key(tenant_main_graph_id())
                        .expect("edge endpoint encoding key");
                    let mut request = ordered_public_request(family, &client_key);
                    let AtomicInsertRequest::V1(request_v1) = &mut request;
                    let AtomicInsertOperationV1::Edge(edge) = &mut request_v1.operations[0] else {
                        unreachable!()
                    };
                    edge.source = AtomicInsertEndpointV1::Existing(
                        gleaph_graph_kernel::federation::encode_global_vertex_id(
                            &encoding_key,
                            gleaph_graph_kernel::federation::GlobalVertexId::new(
                                ShardId::new(0),
                                10,
                            ),
                        )
                        .0
                        .to_vec(),
                    );
                    edge.target = AtomicInsertEndpointV1::Existing(
                        gleaph_graph_kernel::federation::encode_global_vertex_id(
                            &encoding_key,
                            gleaph_graph_kernel::federation::GlobalVertexId::new(
                                ShardId::new(1),
                                11,
                            ),
                        )
                        .0
                        .to_vec(),
                    );
                    (
                        store,
                        request,
                        RouterError::InvalidArgument(
                            "ordered edge item 0 endpoints resolve to different shards".into(),
                        ),
                    )
                }
                OrderedTestFamily::Vertex => (
                    store_with_shards_spec(&[]),
                    ordered_public_request(family, &client_key),
                    RouterError::ShardNotRegistered,
                ),
                OrderedTestFamily::Mixed => {
                    let store = store_with_one_shard();
                    let mut request = ordered_public_request(family, &client_key);
                    let AtomicInsertRequest::V1(request_v1) = &mut request;
                    let AtomicInsertOperationV1::Edge(edge) = &mut request_v1.operations[1] else {
                        unreachable!()
                    };
                    edge.source = AtomicInsertEndpointV1::NewVertexOrdinal(0);
                    edge.target = AtomicInsertEndpointV1::NewVertexOrdinal(0);
                    edge.edge_label_name = Some("missing-edge-label".into());
                    (
                        store,
                        request,
                        RouterError::NotFound("missing-edge-label".into()),
                    )
                }
            };
            let mutation_key =
                mutation_key_for(ordered_test_caller(), tenant_main_graph_id(), &client_key);

            let error = futures::executor::block_on(super::atomic_insert_public(request))
                .expect_err("fresh admission must reject before reservation");

            assert_eq!(error, expected_error, "{family:?}: exact public error");
            assert!(
                store.router_mutation_record(&mutation_key).is_none(),
                "{family:?}: rejection must leave no client-key record, routing reservation, or diagnostic"
            );
        }

        super::set_ordered_test_caller(None);
    }

    #[test]
    fn canonical_pending_reconciliation_adopts_exact_completed_receipts_without_dispatch() {
        let store = store_with_one_shard();
        super::reset_test_reconciliation_dispatches();

        for family in ORDERED_TEST_FAMILIES {
            match family {
                OrderedTestFamily::Edge => {
                    let target = edge_target();
                    let receipt = edge_receipt();
                    let (key, mutation_id) =
                        seed_edge_pending(&store, "exact-edge", [1; 32], target.clone());
                    let entry = completed_journal(
                        mutation_id,
                        GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                            canonical_encoding_version: 1,
                            graph_request_fingerprint: target.graph_request_fingerprint,
                            logical_item_count: 1,
                        },
                        1,
                        None,
                    );
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        mutation_id,
                        Some(entry),
                    );
                    let before = store.router_mutation_record(&key).expect("edge pending");
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &key,
                        &before,
                        CanonicalPendingReconciliationTrigger::BackgroundRecovery,
                        Some(&preflight),
                    ))
                    .expect("exact edge reconciliation");
                    let after = store.router_mutation_record(&key).expect("edge committed");
                    assert_ne!(before.as_v1().payload, after.as_v1().payload);
                    assert_eq!(after.as_v1().last_error, None);
                    assert!(matches!(
                        after.payload(),
                        RouterMutationPayloadV1::OrderedEdgeBatch(replay)
                            if replay.target.request == target.request
                                && replay.target.graph_request_fingerprint
                                    == target.graph_request_fingerprint
                                && replay.target.progress
                                    == OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(receipt)
                    ));
                }
                OrderedTestFamily::Vertex => {
                    let target = vertex_target();
                    let receipt = vertex_receipt();
                    let (key, mutation_id) =
                        seed_vertex_pending(&store, "exact-vertex", [2; 32], target.clone());
                    let entry = completed_journal(
                        mutation_id,
                        GraphMutationRequestIdentityV1::OrderedVertexBatch {
                            canonical_encoding_version: 1,
                            graph_request_fingerprint: target.graph_request_fingerprint,
                            logical_item_count: 1,
                        },
                        1,
                        Some(receipt.allocated_vertex_ids.clone()),
                    );
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        mutation_id,
                        Some(entry),
                    );
                    let before = store.router_mutation_record(&key).expect("vertex pending");
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &key,
                        &before,
                        CanonicalPendingReconciliationTrigger::BackgroundRecovery,
                        Some(&preflight),
                    ))
                    .expect("exact vertex reconciliation");
                    let after = store
                        .router_mutation_record(&key)
                        .expect("vertex committed");
                    assert_ne!(before.as_v1().payload, after.as_v1().payload);
                    assert_eq!(after.as_v1().last_error, None);
                    assert!(matches!(
                        after.payload(),
                        RouterMutationPayloadV1::OrderedVertexBatch(replay)
                            if replay.target.request == target.request
                                && replay.target.graph_request_fingerprint
                                    == target.graph_request_fingerprint
                                && replay.target.progress
                                    == OrderedVertexBatchTargetProgressV1::CanonicalCommitted(receipt)
                    ));
                }
                OrderedTestFamily::Mixed => {
                    let target = mixed_target();
                    let receipt = mixed_receipt();
                    let (key, mutation_id) =
                        seed_mixed_pending(&store, "exact-mixed", [3; 32], target.clone());
                    let entry = completed_journal(
                        mutation_id,
                        GraphMutationRequestIdentityV1::OrderedMixedBatch {
                            canonical_encoding_version: 1,
                            graph_request_fingerprint: target.graph_request_fingerprint,
                            logical_operation_count: 2,
                            logical_vertex_count: 1,
                            logical_edge_count: 1,
                        },
                        2,
                        Some(receipt.allocated_vertex_ids.clone()),
                    );
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        mutation_id,
                        Some(entry),
                    );
                    let before = store.router_mutation_record(&key).expect("mixed pending");
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &key,
                        &before,
                        CanonicalPendingReconciliationTrigger::BackgroundRecovery,
                        Some(&preflight),
                    ))
                    .expect("exact mixed reconciliation");
                    let after = store.router_mutation_record(&key).expect("mixed committed");
                    assert_ne!(before.as_v1().payload, after.as_v1().payload);
                    assert_eq!(after.as_v1().last_error, None);
                    assert!(matches!(
                        after.payload(),
                        RouterMutationPayloadV1::OrderedMixedBatch(replay)
                            if replay.target.request == target.request
                                && replay.target.graph_request_fingerprint
                                    == target.graph_request_fingerprint
                                && replay.target.progress
                                    == OrderedMixedBatchTargetProgressV1::CanonicalCommitted(receipt)
                    ));
                }
            }
            assert_eq!(
                super::test_reconciliation_dispatch_calls(),
                0,
                "{family:?}: exact completed journal evidence must be a local transition"
            );
        }
    }

    #[test]
    fn canonical_pending_reconciliation_rejects_nonexact_evidence_without_dispatch() {
        let store = store_with_one_shard();
        super::reset_test_reconciliation_dispatches();

        for family in ORDERED_TEST_FAMILIES {
            let (key, mutation_id, graph, mut cases) = match family {
                OrderedTestFamily::Edge => {
                    let target = edge_target();
                    let fingerprint = target.graph_request_fingerprint;
                    let (key, mutation_id) =
                        seed_edge_pending(&store, "reject-edge", [11; 32], target.clone());
                    let exact_identity = || GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                        canonical_encoding_version: 1,
                        graph_request_fingerprint: fingerprint,
                        logical_item_count: 1,
                    };
                    let mut invalid = completed_journal(mutation_id, exact_identity(), 1, None);
                    invalid.set_next_index(Some(1));
                    let mut retired = completed_journal(mutation_id, exact_identity(), 1, None);
                    retired.set_retirement(GraphMutationRetirementWireV1::Retired);
                    let mut not_applicable =
                        completed_journal(mutation_id, exact_identity(), 1, None);
                    not_applicable.set_retirement(GraphMutationRetirementWireV1::NotApplicable);
                    let mut incomplete = completed_journal(mutation_id, exact_identity(), 1, None);
                    incomplete.set_state(MutationJournalState::Incomplete);
                    let mut wrong_row = completed_journal(mutation_id, exact_identity(), 1, None);
                    wrong_row.set_row_count(2);
                    let mut wrong_appendix =
                        completed_journal(mutation_id, exact_identity(), 1, None);
                    wrong_appendix.set_allocated_vertex_ids(Some(vec![7]));
                    let cases = vec![
                        ("invalid-wire", invalid),
                        ("retired", retired),
                        ("not-applicable", not_applicable),
                        ("non-completed", incomplete),
                        (
                            "wrong-identity-variant",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedVertexBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 1,
                                },
                                1,
                                Some(vec![7]),
                            ),
                        ),
                        (
                            "wrong-canonical-version",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                                    canonical_encoding_version: 2,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 1,
                                },
                                1,
                                None,
                            ),
                        ),
                        (
                            "wrong-fingerprint",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: [99; 32],
                                    logical_item_count: 1,
                                },
                                1,
                                None,
                            ),
                        ),
                        (
                            "wrong-item-count",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 2,
                                },
                                2,
                                None,
                            ),
                        ),
                        ("wrong-row-count", wrong_row),
                        ("wrong-allocation-appendix", wrong_appendix),
                        (
                            "wrong-mutation-id",
                            completed_journal(mutation_id + 1, exact_identity(), 1, None),
                        ),
                    ];
                    (
                        key,
                        mutation_id,
                        target.request.target_graph_canister,
                        cases,
                    )
                }
                OrderedTestFamily::Vertex => {
                    let target = vertex_target();
                    let fingerprint = target.graph_request_fingerprint;
                    let (key, mutation_id) =
                        seed_vertex_pending(&store, "reject-vertex", [12; 32], target.clone());
                    let exact_identity = || GraphMutationRequestIdentityV1::OrderedVertexBatch {
                        canonical_encoding_version: 1,
                        graph_request_fingerprint: fingerprint,
                        logical_item_count: 1,
                    };
                    let mut invalid =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    invalid.set_next_index(Some(1));
                    let mut retired =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    retired.set_retirement(GraphMutationRetirementWireV1::Retired);
                    let mut not_applicable =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    not_applicable.set_retirement(GraphMutationRetirementWireV1::NotApplicable);
                    let mut incomplete =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    incomplete.set_state(MutationJournalState::Incomplete);
                    let mut wrong_row =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    wrong_row.set_row_count(2);
                    let mut missing_appendix =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    missing_appendix.set_allocated_vertex_ids(None);
                    let mut wrong_appendix =
                        completed_journal(mutation_id, exact_identity(), 1, Some(vec![7]));
                    wrong_appendix.set_allocated_vertex_ids(Some(vec![7, 8]));
                    let cases = vec![
                        ("invalid-wire", invalid),
                        ("retired", retired),
                        ("not-applicable", not_applicable),
                        ("non-completed", incomplete),
                        (
                            "wrong-identity-variant",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 1,
                                },
                                1,
                                None,
                            ),
                        ),
                        (
                            "wrong-canonical-version",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedVertexBatch {
                                    canonical_encoding_version: 2,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 1,
                                },
                                1,
                                Some(vec![7]),
                            ),
                        ),
                        (
                            "wrong-fingerprint",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedVertexBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: [99; 32],
                                    logical_item_count: 1,
                                },
                                1,
                                Some(vec![7]),
                            ),
                        ),
                        (
                            "wrong-item-count",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedVertexBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 2,
                                },
                                2,
                                Some(vec![7, 8]),
                            ),
                        ),
                        ("wrong-row-count", wrong_row),
                        ("missing-allocation-appendix", missing_appendix),
                        ("wrong-allocation-appendix", wrong_appendix),
                        (
                            "wrong-mutation-id",
                            completed_journal(mutation_id + 1, exact_identity(), 1, Some(vec![7])),
                        ),
                    ];
                    (
                        key,
                        mutation_id,
                        target.request.target_graph_canister,
                        cases,
                    )
                }
                OrderedTestFamily::Mixed => {
                    let target = mixed_target();
                    let fingerprint = target.graph_request_fingerprint;
                    let (key, mutation_id) =
                        seed_mixed_pending(&store, "reject-mixed", [13; 32], target.clone());
                    let exact_identity = || GraphMutationRequestIdentityV1::OrderedMixedBatch {
                        canonical_encoding_version: 1,
                        graph_request_fingerprint: fingerprint,
                        logical_operation_count: 2,
                        logical_vertex_count: 1,
                        logical_edge_count: 1,
                    };
                    let mut invalid =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    invalid.set_next_index(Some(1));
                    let mut retired =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    retired.set_retirement(GraphMutationRetirementWireV1::Retired);
                    let mut not_applicable =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    not_applicable.set_retirement(GraphMutationRetirementWireV1::NotApplicable);
                    let mut incomplete =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    incomplete.set_state(MutationJournalState::Incomplete);
                    let mut wrong_row =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    wrong_row.set_row_count(3);
                    let mut missing_appendix =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    missing_appendix.set_allocated_vertex_ids(None);
                    let mut wrong_appendix =
                        completed_journal(mutation_id, exact_identity(), 2, Some(vec![7]));
                    wrong_appendix.set_allocated_vertex_ids(Some(vec![7, 8]));
                    let cases = vec![
                        ("invalid-wire", invalid),
                        ("retired", retired),
                        ("not-applicable", not_applicable),
                        ("non-completed", incomplete),
                        (
                            "wrong-identity-variant",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_item_count: 2,
                                },
                                2,
                                None,
                            ),
                        ),
                        (
                            "wrong-canonical-version",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedMixedBatch {
                                    canonical_encoding_version: 2,
                                    graph_request_fingerprint: fingerprint,
                                    logical_operation_count: 2,
                                    logical_vertex_count: 1,
                                    logical_edge_count: 1,
                                },
                                2,
                                Some(vec![7]),
                            ),
                        ),
                        (
                            "wrong-fingerprint",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedMixedBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: [99; 32],
                                    logical_operation_count: 2,
                                    logical_vertex_count: 1,
                                    logical_edge_count: 1,
                                },
                                2,
                                Some(vec![7]),
                            ),
                        ),
                        (
                            "wrong-operation-count",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedMixedBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_operation_count: 3,
                                    logical_vertex_count: 1,
                                    logical_edge_count: 2,
                                },
                                3,
                                Some(vec![7]),
                            ),
                        ),
                        (
                            "wrong-vertex-count",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedMixedBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_operation_count: 2,
                                    logical_vertex_count: 0,
                                    logical_edge_count: 2,
                                },
                                2,
                                Some(Vec::new()),
                            ),
                        ),
                        (
                            "wrong-edge-count",
                            completed_journal(
                                mutation_id,
                                GraphMutationRequestIdentityV1::OrderedMixedBatch {
                                    canonical_encoding_version: 1,
                                    graph_request_fingerprint: fingerprint,
                                    logical_operation_count: 2,
                                    logical_vertex_count: 2,
                                    logical_edge_count: 0,
                                },
                                2,
                                Some(vec![7, 8]),
                            ),
                        ),
                        ("wrong-row-count", wrong_row),
                        ("missing-allocation-appendix", missing_appendix),
                        ("wrong-allocation-appendix", wrong_appendix),
                        (
                            "wrong-mutation-id",
                            completed_journal(mutation_id + 1, exact_identity(), 2, Some(vec![7])),
                        ),
                    ];
                    (
                        key,
                        mutation_id,
                        target.request.target_graph_canister,
                        cases,
                    )
                }
            };

            for (case, entry) in cases.drain(..) {
                let label = format!("{family:?}/{case}");
                let before = store.router_mutation_record(&key).expect("pending record");
                let preflight = preflight_with_journal(graph, mutation_id, Some(entry));
                futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                    &store,
                    &key,
                    &before,
                    CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                    Some(&preflight),
                ))
                .expect("nonexact evidence remains a diagnostic outcome");
                let after = store
                    .router_mutation_record(&key)
                    .expect("pending record after case");
                assert_pending_diagnostic(&before, &after, &label);
                assert_eq!(
                    super::test_reconciliation_dispatch_calls(),
                    0,
                    "{label}: nonexact evidence must never dispatch"
                );
            }
        }
    }

    #[test]
    fn canonical_pending_reconciliation_handles_absent_by_trigger() {
        let store = store_with_one_shard();

        for family in ORDERED_TEST_FAMILIES {
            super::reset_test_reconciliation_dispatches();
            for trigger in [
                CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                CanonicalPendingReconciliationTrigger::BackgroundRecovery,
            ] {
                let client_key = format!("journal-query-error-{family:?}-{trigger:?}");
                let (key, target_graph) = match family {
                    OrderedTestFamily::Edge => {
                        let target = edge_target();
                        let target_graph = target.request.target_graph_canister;
                        let (key, _) = seed_edge_pending(&store, &client_key, [31; 32], target);
                        (key, target_graph)
                    }
                    OrderedTestFamily::Vertex => {
                        let target = vertex_target();
                        let target_graph = target.request.target_graph_canister;
                        let (key, _) = seed_vertex_pending(&store, &client_key, [32; 32], target);
                        (key, target_graph)
                    }
                    OrderedTestFamily::Mixed => {
                        let target = mixed_target();
                        let target_graph = target.request.target_graph_canister;
                        let (key, _) = seed_mixed_pending(&store, &client_key, [33; 32], target);
                        (key, target_graph)
                    }
                };
                let before = store
                    .router_mutation_record(&key)
                    .expect("query-error pending record");
                futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                    &store, &key, &before, trigger, None,
                ))
                .expect("ambiguous journal query error remains diagnostic");
                let after = store
                    .router_mutation_record(&key)
                    .expect("query-error pending record after reconciliation");
                assert_pending_diagnostic(
                    &before,
                    &after,
                    &format!("{family:?}/{trigger:?}/journal-query-error/{target_graph}"),
                );
                let diagnostic = after
                    .as_v1()
                    .last_error
                    .as_deref()
                    .expect("query error diagnostic");
                assert!(diagnostic.contains("get_mutation_journal_entry unavailable"));
                assert!(!diagnostic.contains("journal entry is absent"));
                assert_eq!(
                    super::test_reconciliation_dispatch_calls(),
                    0,
                    "{family:?}/{trigger:?}: ambiguous query failure must never redispatch"
                );
            }

            match family {
                OrderedTestFamily::Edge => {
                    let target = edge_target();
                    let (timer_key, timer_mutation_id) =
                        seed_edge_pending(&store, "absent-timer-edge", [21; 32], target.clone());
                    let before = store
                        .router_mutation_record(&timer_key)
                        .expect("timer edge");
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        timer_mutation_id,
                        None,
                    );
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &timer_key,
                        &before,
                        CanonicalPendingReconciliationTrigger::BackgroundRecovery,
                        Some(&preflight),
                    ))
                    .expect("timer edge absence");
                    let after = store
                        .router_mutation_record(&timer_key)
                        .expect("timer edge after");
                    assert_pending_diagnostic(&before, &after, "edge timer absent");
                    assert_eq!(super::test_reconciliation_dispatch_calls(), 0);

                    let (retry_key, retry_mutation_id) =
                        seed_edge_pending(&store, "absent-retry-edge", [22; 32], target.clone());
                    let receipt = edge_receipt();
                    super::set_test_edge_dispatch(Ok(GraphOrderedEdgeBatchResult::V1(
                        GraphOrderedEdgeBatchResultV1::Completed(receipt.clone()),
                    )));
                    let before = store
                        .router_mutation_record(&retry_key)
                        .expect("retry edge");
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        retry_mutation_id,
                        None,
                    );
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &retry_key,
                        &before,
                        CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                        Some(&preflight),
                    ))
                    .expect("retry edge absence");
                    assert_eq!(super::test_reconciliation_dispatch_calls(), 1);
                    assert_eq!(
                        super::test_edge_dispatch_args(),
                        Some(stored_edge_dispatch_args(retry_mutation_id, &target))
                    );
                    let after = store
                        .router_mutation_record(&retry_key)
                        .expect("retry edge after");
                    assert!(matches!(
                        after.payload(),
                        RouterMutationPayloadV1::OrderedEdgeBatch(replay)
                            if replay.target.progress
                                == OrderedEdgeBatchTargetProgressV1::CanonicalCommitted(receipt)
                    ));

                    for case in ["noncompleted-wrong-fingerprint", "wrong-count"] {
                        super::reset_test_reconciliation_dispatches();
                        let client_key = format!("absent-retry-edge-{case}");
                        let (key, mutation_id) =
                            seed_edge_pending(&store, &client_key, [41; 32], target.clone());
                        let result = match case {
                            "noncompleted-wrong-fingerprint" => GraphOrderedEdgeBatchResult::V1(
                                GraphOrderedEdgeBatchResultV1::MutationRetired {
                                    mutation_id: mutation_id + 1,
                                    graph_request_fingerprint: [99; 32],
                                },
                            ),
                            "wrong-count" => {
                                let mut receipt = edge_receipt();
                                receipt.logical_edge_count = 2;
                                GraphOrderedEdgeBatchResult::V1(
                                    GraphOrderedEdgeBatchResultV1::Completed(receipt),
                                )
                            }
                            _ => unreachable!(),
                        };
                        super::set_test_edge_dispatch(Ok(result));
                        let before = store
                            .router_mutation_record(&key)
                            .expect("wrong edge result pending record");
                        let preflight = preflight_with_journal(
                            target.request.target_graph_canister,
                            mutation_id,
                            None,
                        );
                        futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                            &store,
                            &key,
                            &before,
                            CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                            Some(&preflight),
                        ))
                        .expect("wrong edge redispatch result remains diagnostic");
                        let after = store
                            .router_mutation_record(&key)
                            .expect("wrong edge result pending record after redispatch");
                        assert_pending_diagnostic(
                            &before,
                            &after,
                            &format!("edge redispatch/{case}"),
                        );
                        assert_eq!(super::test_reconciliation_dispatch_calls(), 1);
                        assert_eq!(
                            super::test_edge_dispatch_args(),
                            Some(stored_edge_dispatch_args(mutation_id, &target))
                        );
                    }
                }
                OrderedTestFamily::Vertex => {
                    let target = vertex_target();
                    let (timer_key, timer_mutation_id) = seed_vertex_pending(
                        &store,
                        "absent-timer-vertex",
                        [23; 32],
                        target.clone(),
                    );
                    let before = store
                        .router_mutation_record(&timer_key)
                        .expect("timer vertex");
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        timer_mutation_id,
                        None,
                    );
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &timer_key,
                        &before,
                        CanonicalPendingReconciliationTrigger::BackgroundRecovery,
                        Some(&preflight),
                    ))
                    .expect("timer vertex absence");
                    let after = store
                        .router_mutation_record(&timer_key)
                        .expect("timer vertex after");
                    assert_pending_diagnostic(&before, &after, "vertex timer absent");
                    assert_eq!(super::test_reconciliation_dispatch_calls(), 0);

                    let (retry_key, retry_mutation_id) = seed_vertex_pending(
                        &store,
                        "absent-retry-vertex",
                        [24; 32],
                        target.clone(),
                    );
                    let receipt = vertex_receipt();
                    super::set_test_vertex_dispatch(Ok(GraphOrderedVertexBatchResult::V1(
                        GraphOrderedVertexBatchResultV1::Completed(receipt.clone()),
                    )));
                    let before = store
                        .router_mutation_record(&retry_key)
                        .expect("retry vertex");
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        retry_mutation_id,
                        None,
                    );
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &retry_key,
                        &before,
                        CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                        Some(&preflight),
                    ))
                    .expect("retry vertex absence");
                    assert_eq!(super::test_reconciliation_dispatch_calls(), 1);
                    assert_eq!(
                        super::test_vertex_dispatch_args(),
                        Some(stored_vertex_dispatch_args(retry_mutation_id, &target))
                    );
                    let after = store
                        .router_mutation_record(&retry_key)
                        .expect("retry vertex after");
                    assert!(matches!(
                        after.payload(),
                        RouterMutationPayloadV1::OrderedVertexBatch(replay)
                            if replay.target.progress
                                == OrderedVertexBatchTargetProgressV1::CanonicalCommitted(receipt)
                    ));

                    for case in [
                        "noncompleted-wrong-fingerprint",
                        "wrong-count",
                        "wrong-allocation",
                    ] {
                        super::reset_test_reconciliation_dispatches();
                        let client_key = format!("absent-retry-vertex-{case}");
                        let (key, mutation_id) =
                            seed_vertex_pending(&store, &client_key, [42; 32], target.clone());
                        let result = match case {
                            "noncompleted-wrong-fingerprint" => GraphOrderedVertexBatchResult::V1(
                                GraphOrderedVertexBatchResultV1::MutationRetired {
                                    mutation_id: mutation_id + 1,
                                    graph_request_fingerprint: [98; 32],
                                },
                            ),
                            "wrong-count" => GraphOrderedVertexBatchResult::V1(
                                GraphOrderedVertexBatchResultV1::Completed(
                                    GraphOrderedVertexBatchReceiptV1 {
                                        logical_vertex_count: 2,
                                        emitted_delta_first_seq: None,
                                        emitted_delta_last_seq: None,
                                        hot_forward_vertices: Vec::new(),
                                        allocated_vertex_ids: vec![7, 8],
                                    },
                                ),
                            ),
                            "wrong-allocation" => GraphOrderedVertexBatchResult::V1(
                                GraphOrderedVertexBatchResultV1::Completed(
                                    GraphOrderedVertexBatchReceiptV1 {
                                        logical_vertex_count: 1,
                                        emitted_delta_first_seq: None,
                                        emitted_delta_last_seq: None,
                                        hot_forward_vertices: Vec::new(),
                                        allocated_vertex_ids: Vec::new(),
                                    },
                                ),
                            ),
                            _ => unreachable!(),
                        };
                        super::set_test_vertex_dispatch(Ok(result));
                        let before = store
                            .router_mutation_record(&key)
                            .expect("wrong vertex result pending record");
                        let preflight = preflight_with_journal(
                            target.request.target_graph_canister,
                            mutation_id,
                            None,
                        );
                        futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                            &store,
                            &key,
                            &before,
                            CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                            Some(&preflight),
                        ))
                        .expect("wrong vertex redispatch result remains diagnostic");
                        let after = store
                            .router_mutation_record(&key)
                            .expect("wrong vertex result pending record after redispatch");
                        assert_pending_diagnostic(
                            &before,
                            &after,
                            &format!("vertex redispatch/{case}"),
                        );
                        assert_eq!(super::test_reconciliation_dispatch_calls(), 1);
                        assert_eq!(
                            super::test_vertex_dispatch_args(),
                            Some(stored_vertex_dispatch_args(mutation_id, &target))
                        );
                    }
                }
                OrderedTestFamily::Mixed => {
                    let target = mixed_target();
                    let (timer_key, timer_mutation_id) =
                        seed_mixed_pending(&store, "absent-timer-mixed", [25; 32], target.clone());
                    let before = store
                        .router_mutation_record(&timer_key)
                        .expect("timer mixed");
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        timer_mutation_id,
                        None,
                    );
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &timer_key,
                        &before,
                        CanonicalPendingReconciliationTrigger::BackgroundRecovery,
                        Some(&preflight),
                    ))
                    .expect("timer mixed absence");
                    let after = store
                        .router_mutation_record(&timer_key)
                        .expect("timer mixed after");
                    assert_pending_diagnostic(&before, &after, "mixed timer absent");
                    assert_eq!(super::test_reconciliation_dispatch_calls(), 0);

                    let (retry_key, retry_mutation_id) =
                        seed_mixed_pending(&store, "absent-retry-mixed", [26; 32], target.clone());
                    let receipt = mixed_receipt();
                    super::set_test_mixed_dispatch(Ok(GraphOrderedMixedBatchResult::V1(
                        GraphOrderedMixedBatchResultV1::Completed(receipt.clone()),
                    )));
                    let before = store
                        .router_mutation_record(&retry_key)
                        .expect("retry mixed");
                    let preflight = preflight_with_journal(
                        target.request.target_graph_canister,
                        retry_mutation_id,
                        None,
                    );
                    futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                        &store,
                        &retry_key,
                        &before,
                        CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                        Some(&preflight),
                    ))
                    .expect("retry mixed absence");
                    assert_eq!(super::test_reconciliation_dispatch_calls(), 1);
                    assert_eq!(
                        super::test_mixed_dispatch_args(),
                        Some(stored_mixed_dispatch_args(retry_mutation_id, &target))
                    );
                    let after = store
                        .router_mutation_record(&retry_key)
                        .expect("retry mixed after");
                    assert!(matches!(
                        after.payload(),
                        RouterMutationPayloadV1::OrderedMixedBatch(replay)
                            if replay.target.progress
                                == OrderedMixedBatchTargetProgressV1::CanonicalCommitted(receipt)
                    ));

                    for case in [
                        "noncompleted-wrong-fingerprint",
                        "wrong-counts",
                        "wrong-allocation",
                    ] {
                        super::reset_test_reconciliation_dispatches();
                        let client_key = format!("absent-retry-mixed-{case}");
                        let (key, mutation_id) =
                            seed_mixed_pending(&store, &client_key, [43; 32], target.clone());
                        let result = match case {
                            "noncompleted-wrong-fingerprint" => GraphOrderedMixedBatchResult::V1(
                                GraphOrderedMixedBatchResultV1::MutationRetired {
                                    mutation_id: mutation_id + 1,
                                    graph_request_fingerprint: [97; 32],
                                },
                            ),
                            "wrong-counts" => GraphOrderedMixedBatchResult::V1(
                                GraphOrderedMixedBatchResultV1::Completed(
                                    GraphOrderedMixedBatchReceiptV1 {
                                        logical_operation_count: 3,
                                        logical_vertex_count: 1,
                                        logical_edge_count: 2,
                                        emitted_delta_first_seq: None,
                                        emitted_delta_last_seq: None,
                                        hot_forward_vertices: Vec::new(),
                                        allocated_vertex_ids: vec![7],
                                    },
                                ),
                            ),
                            "wrong-allocation" => GraphOrderedMixedBatchResult::V1(
                                GraphOrderedMixedBatchResultV1::Completed(
                                    GraphOrderedMixedBatchReceiptV1 {
                                        logical_operation_count: 2,
                                        logical_vertex_count: 1,
                                        logical_edge_count: 1,
                                        emitted_delta_first_seq: None,
                                        emitted_delta_last_seq: None,
                                        hot_forward_vertices: Vec::new(),
                                        allocated_vertex_ids: Vec::new(),
                                    },
                                ),
                            ),
                            _ => unreachable!(),
                        };
                        super::set_test_mixed_dispatch(Ok(result));
                        let before = store
                            .router_mutation_record(&key)
                            .expect("wrong mixed result pending record");
                        let preflight = preflight_with_journal(
                            target.request.target_graph_canister,
                            mutation_id,
                            None,
                        );
                        futures::executor::block_on(super::reconcile_ordered_canonical_pending(
                            &store,
                            &key,
                            &before,
                            CanonicalPendingReconciliationTrigger::ExplicitSameKeyRetry,
                            Some(&preflight),
                        ))
                        .expect("wrong mixed redispatch result remains diagnostic");
                        let after = store
                            .router_mutation_record(&key)
                            .expect("wrong mixed result pending record after redispatch");
                        assert_pending_diagnostic(
                            &before,
                            &after,
                            &format!("mixed redispatch/{case}"),
                        );
                        assert_eq!(super::test_reconciliation_dispatch_calls(), 1);
                        assert_eq!(
                            super::test_mixed_dispatch_args(),
                            Some(stored_mixed_dispatch_args(mutation_id, &target))
                        );
                    }
                }
            }
        }
    }

    fn parse_block(query: &str) -> gleaph_gql::ast::StatementBlock {
        let program = parser::parse(query).expect("parse query");
        program
            .transaction_activity
            .expect("transaction activity")
            .body
            .expect("statement block")
    }

    /// ADR 0029 §6 (Phase 5): zero/one top-level DML statements always pass the gate,
    /// regardless of whether the target graph is single- or multi-shard.
    #[test]
    fn multi_dml_gate_zero_or_one_dml_passes() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();

        assert!(enforce_multi_dml_bundle_gate(&store, graph_id, &parse_block("RETURN 1")).is_ok());
        assert!(
            enforce_multi_dml_bundle_gate(&store, graph_id, &parse_block("INSERT (a)")).is_ok()
        );
    }

    /// ADR 0029 §6 (Phase 5): multiple top-level DML statements are allowed when the
    /// graph has exactly one live shard, preserving shard-local atomicity.
    #[test]
    fn multi_dml_gate_single_shard_multi_dml_passes() {
        let store = store_with_one_shard();
        let graph_id = tenant_main_graph_id();
        let block = parse_block("INSERT (a) NEXT INSERT (b)");

        assert!(enforce_multi_dml_bundle_gate(&store, graph_id, &block).is_ok());
    }

    /// ADR 0029 §6 (Phase 5): federated graphs reject multiple top-level DML statements
    /// before any shard dispatch, returning the exact DML and shard counts.
    #[test]
    fn multi_dml_gate_federated_multi_dml_rejects_with_exact_counts() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();

        let err = enforce_multi_dml_bundle_gate(
            &store,
            graph_id,
            &parse_block("INSERT (a) NEXT INSERT (b)"),
        )
        .expect_err("two DMLs on two shards must be rejected");
        assert!(
            matches!(
                err,
                RouterError::UnsupportedMultiDmlBundle {
                    dml_statements: 2,
                    shard_count: 2,
                }
            ),
            "unexpected error: {err:?}"
        );

        let err = enforce_multi_dml_bundle_gate(
            &store,
            graph_id,
            &parse_block("INSERT (a) NEXT INSERT (b) NEXT INSERT (c)"),
        )
        .expect_err("three DMLs on two shards must be rejected");
        assert!(
            matches!(
                err,
                RouterError::UnsupportedMultiDmlBundle {
                    dml_statements: 3,
                    shard_count: 2,
                }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[derive(Clone)]
    struct FakeIndex {
        calls: Rc<Cell<u32>>,
        results: Rc<RefCell<Vec<Result<Vec<PostingHit>, String>>>>,
    }

    impl FakeIndex {
        fn new(results: Vec<Result<Vec<PostingHit>, String>>) -> Self {
            Self {
                calls: Rc::new(Cell::new(0)),
                results: Rc::new(RefCell::new(results)),
            }
        }

        fn calls(&self) -> u32 {
            self.calls.get()
        }
    }

    impl IndexLookup for FakeIndex {
        fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.calls.set(self.calls.get() + 1);
            let result = self.results.borrow_mut().remove(0);
            Box::pin(async move { result })
        }

        fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _range: gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.calls.set(self.calls.get() + 1);
            let result = self.results.borrow_mut().remove(0);
            Box::pin(async move { result })
        }

        fn lookup_intersection(
            &self,
            _req: gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            gleaph_graph_kernel::index::IndexIntersectionResult,
                            String,
                        >,
                    > + '_,
            >,
        > {
            self.calls.set(self.calls.get() + 1);
            let result = self
                .results
                .borrow_mut()
                .remove(0)
                .map(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices);
            Box::pin(async move { result })
        }

        fn lookup_edge_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_edge_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _range: gleaph_graph_kernel::index::PostingRangeRequest,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _min_count: u64,
            _vertex_filter_packed: Option<Vec<u64>>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_label_intersection(
            &self,
            _req: IndexLabelIntersectionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.calls.set(self.calls.get() + 1);
            let result = self.results.borrow_mut().remove(0);
            Box::pin(async move { result })
        }

        fn lookup_label_page(
            &self,
            _req: gleaph_graph_kernel::index::LabelLookupPageRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
            Box::pin(async move {
                Ok(LabelLookupPageResult {
                    hits: Vec::new(),
                    next: None,
                    done: true,
                })
            })
        }

        fn filter_hits_by_label(
            &self,
            _vertex_label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(hits) })
        }

        fn count_postings_by_value_for_label(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _vertex_label_id: u32,
            _min_count: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    #[derive(Clone)]
    struct LabelIntersectionFakeIndex {
        pages: Rc<RefCell<Vec<(ShardId, u32, LabelLookupPageResult)>>>,
        sieve_keep: Rc<RefCell<Vec<u32>>>,
        page_calls: Rc<Cell<u32>>,
        sieve_calls: Rc<Cell<u32>>,
    }

    impl LabelIntersectionFakeIndex {
        fn new(pages: Vec<(ShardId, u32, LabelLookupPageResult)>, sieve_keep: Vec<u32>) -> Self {
            Self {
                pages: Rc::new(RefCell::new(pages)),
                sieve_keep: Rc::new(RefCell::new(sieve_keep)),
                page_calls: Rc::new(Cell::new(0)),
                sieve_calls: Rc::new(Cell::new(0)),
            }
        }

        fn page_calls(&self) -> u32 {
            self.page_calls.get()
        }

        fn sieve_calls(&self) -> u32 {
            self.sieve_calls.get()
        }
    }

    impl IndexLookup for LabelIntersectionFakeIndex {
        fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _range: gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_intersection(
            &self,
            _req: gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            gleaph_graph_kernel::index::IndexIntersectionResult,
                            String,
                        >,
                    > + '_,
            >,
        > {
            Box::pin(async move {
                Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(Vec::new()))
            })
        }

        fn lookup_edge_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_edge_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _range: gleaph_graph_kernel::index::PostingRangeRequest,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _min_count: u64,
            _vertex_filter_packed: Option<Vec<u64>>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_label_intersection(
            &self,
            _req: IndexLabelIntersectionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_label_page(
            &self,
            req: gleaph_graph_kernel::index::LabelLookupPageRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
            self.page_calls.set(self.page_calls.get() + 1);
            let mut pages = self.pages.borrow_mut();
            if let Some(pos) = pages.iter().position(|(shard_id, label_id, _)| {
                *shard_id == req.shard_id && *label_id == req.vertex_label_id && req.after.is_none()
            }) {
                let (_, _, page) = pages.remove(pos);
                return Box::pin(async move { Ok(page) });
            }
            Box::pin(async move {
                Ok(LabelLookupPageResult {
                    hits: Vec::new(),
                    next: None,
                    done: true,
                })
            })
        }

        fn filter_hits_by_label(
            &self,
            _vertex_label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.sieve_calls.set(self.sieve_calls.get() + 1);
            let keep = self.sieve_keep.borrow().clone();
            Box::pin(async move {
                Ok(hits
                    .into_iter()
                    .filter(|hit| keep.contains(&hit.vertex_id))
                    .collect())
            })
        }

        fn count_postings_by_value_for_label(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _vertex_label_id: u32,
            _min_count: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    fn store_with_person_employee_labels() -> RouterStore {
        let store = store_with_shards();
        let admin = Principal::from_slice(&[1; 29]);
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Person")
            .expect("intern Person");
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Employee")
            .expect("intern Employee");
        store
    }

    fn label_intersection_read_plan() -> PhysicalPlan {
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql::types::LabelExpr;
        use gleaph_gql_planner::plan::ProjectColumn;

        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::IsLabeled {
                    expr: Box::new(Expr::var("n")),
                    label: LabelExpr::Name("Employee".into()),
                    negated: false,
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::var("n"),
                    alias: Some(Rc::from("n")),
                }],
                distinct: false,
            },
        ])
    }

    fn label_intersection_fake_index() -> LabelIntersectionFakeIndex {
        LabelIntersectionFakeIndex::new(
            vec![
                (
                    ShardId::new(0),
                    1,
                    LabelLookupPageResult {
                        hits: vec![
                            PostingHit {
                                shard_id: ShardId::new(0),
                                vertex_id: 10,
                            },
                            PostingHit {
                                shard_id: ShardId::new(0),
                                vertex_id: 11,
                            },
                        ],
                        next: None,
                        done: true,
                    },
                ),
                (
                    ShardId::new(1),
                    1,
                    LabelLookupPageResult {
                        hits: vec![PostingHit {
                            shard_id: ShardId::new(1),
                            vertex_id: 20,
                        }],
                        next: None,
                        done: true,
                    },
                ),
            ],
            vec![10, 20],
        )
    }

    #[derive(Clone)]
    struct CompoundSeedFakeIndex {
        label_pages: Rc<RefCell<Vec<(ShardId, u32, LabelLookupPageResult)>>>,
        equal_hits: Vec<PostingHit>,
        page_calls: Rc<Cell<u32>>,
        equal_calls: Rc<Cell<u32>>,
    }

    impl CompoundSeedFakeIndex {
        fn new(
            label_pages: Vec<(ShardId, u32, LabelLookupPageResult)>,
            equal_hits: Vec<PostingHit>,
        ) -> Self {
            Self {
                label_pages: Rc::new(RefCell::new(label_pages)),
                equal_hits,
                page_calls: Rc::new(Cell::new(0)),
                equal_calls: Rc::new(Cell::new(0)),
            }
        }

        fn page_calls(&self) -> u32 {
            self.page_calls.get()
        }

        fn equal_calls(&self) -> u32 {
            self.equal_calls.get()
        }
    }

    impl IndexLookup for CompoundSeedFakeIndex {
        fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.equal_calls.set(self.equal_calls.get() + 1);
            let hits = self.equal_hits.clone();
            Box::pin(async move { Ok(hits) })
        }

        fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _range: gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_intersection(
            &self,
            _req: gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            gleaph_graph_kernel::index::IndexIntersectionResult,
                            String,
                        >,
                    > + '_,
            >,
        > {
            Box::pin(async move {
                Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(Vec::new()))
            })
        }

        fn lookup_edge_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_edge_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _range: gleaph_graph_kernel::index::PostingRangeRequest,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _min_count: u64,
            _vertex_filter_packed: Option<Vec<u64>>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_label_intersection(
            &self,
            _req: IndexLabelIntersectionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_label_page(
            &self,
            req: gleaph_graph_kernel::index::LabelLookupPageRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
            self.page_calls.set(self.page_calls.get() + 1);
            let mut pages = self.label_pages.borrow_mut();
            if let Some(pos) = pages.iter().position(|(shard_id, label_id, _)| {
                *shard_id == req.shard_id && *label_id == req.vertex_label_id && req.after.is_none()
            }) {
                let (_, _, page) = pages.remove(pos);
                return Box::pin(async move { Ok(page) });
            }
            Box::pin(async move {
                Ok(LabelLookupPageResult {
                    hits: Vec::new(),
                    next: None,
                    done: true,
                })
            })
        }

        fn filter_hits_by_label(
            &self,
            _vertex_label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(hits) })
        }

        fn count_postings_by_value_for_label(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _vertex_label_id: u32,
            _min_count: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    fn store_with_person_and_region_property() -> RouterStore {
        let store = store_with_shards();
        let admin = Principal::from_slice(&[1; 29]);
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Person")
            .expect("intern Person");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.main",
            "region",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store,
            tenant_main_graph_id(),
            0,
            "region",
        );
        store
    }

    fn compound_label_property_read_plan() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::NodeScan {
                variable: Rc::from("n"),
                label: Some(NodeLabelRef::from("Person")),
                property_projection: None,
            },
            PlanOp::IndexScan {
                variable: Rc::from("n"),
                property: Rc::from("region"),
                value: ScanValue::Literal(Value::Text("US".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
                ordered_by_sort: None,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ])
    }

    fn compound_seed_fake_index() -> CompoundSeedFakeIndex {
        CompoundSeedFakeIndex::new(
            vec![(
                ShardId::new(0),
                1,
                LabelLookupPageResult {
                    hits: vec![
                        PostingHit {
                            shard_id: ShardId::new(0),
                            vertex_id: 10,
                        },
                        PostingHit {
                            shard_id: ShardId::new(0),
                            vertex_id: 11,
                        },
                    ],
                    next: None,
                    done: true,
                },
            )],
            vec![
                PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 10,
                },
                PostingHit {
                    shard_id: ShardId::new(1),
                    vertex_id: 20,
                },
            ],
        )
    }

    fn seeded_dml_plan() -> PhysicalPlan {
        PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: Rc::from("u"),
                property: Rc::from("uid"),
                value: ScanValue::Literal(Value::Text("alice".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
                ordered_by_sort: None,
            },
            PlanOp::InsertVertex {
                variable: Some(Rc::from("n")),
                labels: vec![NodeLabelRef::from("Person")],
                properties: vec![],
            },
        ])
    }

    fn seeded_dml_bundle(plan: &PhysicalPlan) -> Vec<u8> {
        encode_block_plans(std::slice::from_ref(plan), true).expect("encode plan")
    }

    fn store_with_shards_and_property() -> RouterStore {
        let store = store_with_shards();
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.main",
            "uid",
        );
        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store,
            tenant_main_graph_id(),
            0,
            "uid",
        );
        store
    }

    async fn dispatch_with_fake_index(
        store: &RouterStore,
        fake_index: &FakeIndex,
        plan: &PhysicalPlan,
        plan_blob: &[u8],
        client_key: &str,
    ) -> Result<GqlQueryResult, RouterError> {
        let graph_id = tenant_main_graph_id();
        let shards = store.list_live_shards_for_graph_id(graph_id)?;
        dispatch_plan_blob_with_index(
            graph_id,
            plan_blob,
            std::slice::from_ref(plan),
            &BTreeMap::new(),
            &[],
            GqlExecutionMode::Update,
            Some(client_key),
            store,
            shards,
            fake_index,
            Principal::anonymous(),
            &tenant_main_stats(),
            None,
        )
        .await
    }

    #[test]
    fn pre_dispatch_index_failure_releases_routing_owner_but_preserves_key_record() {
        let store = store_with_shards_and_property();
        let plan = seeded_dml_plan();
        let plan_blob = seeded_dml_bundle(&plan);
        let fake_index = FakeIndex::new(vec![Err("index unavailable".into())]);

        let err = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect_err("index failure");
        assert_eq!(
            err,
            RouterError::InvalidArgument("index unavailable".into())
        );
        assert_eq!(fake_index.calls(), 1);

        let record = store
            .router_mutation_record(&mutation_key_for(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
            ))
            .expect("mutation record");
        assert_eq!(record.as_v1().mutation_id, 1);
        assert_eq!(
            record.as_v1().request_identity.request_fingerprint(),
            request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update)
        );
        assert!(!record.as_v1().routing_in_progress);
        assert!(record.shards().is_empty());
        assert!(record.as_v1().completed_row_count.is_none());

        let retry = store
            .reserve_mutation_id_for_client_key(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
                request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update),
            )
            .expect("retry reservation");
        assert_eq!(retry.mutation_id, record.as_v1().mutation_id);
        assert!(retry.routing_owner);
        assert_eq!(
            store.reserve_mutation_id_for_client_key(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
                b"different request".to_vec(),
            ),
            Err(RouterError::Conflict(
                "client_mutation_key was already used for a different request".into()
            ))
        );
    }

    #[test]
    fn gql_query_result_from_merged_carries_rows_blob() {
        let rows_blob = GqlWireRows {
            rows: vec![gleaph_gql_ic::GqlWireRow {
                columns: vec![("n".into(), GqlWireValue::Int64(7))],
            }],
        }
        .encode_blob()
        .expect("encode");
        let merged = ExecutePlanResult {
            row_count: 1,
            rows_blob: Some(rows_blob.clone()),
            hot_forward_vertices: Vec::new(),
        };
        let out = GqlQueryResult::from_merged(&merged);
        assert_eq!(out.row_count, 1);
        assert_eq!(out.rows_blob, Some(rows_blob));
    }

    #[test]
    fn zero_hit_seeded_dml_records_completed_zero_rows() {
        let store = store_with_shards_and_property();
        let plan = seeded_dml_plan();
        let plan_blob = seeded_dml_bundle(&plan);
        let fake_index = FakeIndex::new(vec![Ok(Vec::new())]);

        let rows = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect("zero-hit dispatch");
        assert_eq!(rows.row_count, 0);
        assert!(rows.rows_blob.is_none());
        assert_eq!(fake_index.calls(), 1);

        let record = store
            .router_mutation_record(&mutation_key_for(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
            ))
            .expect("mutation record");
        assert_eq!(record.as_v1().completed_row_count, Some(0));
        assert!(!record.as_v1().routing_in_progress);
        assert!(record.shards().is_empty());

        let rows = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect("cached zero-hit retry");
        assert_eq!(rows.row_count, 0);
        assert_eq!(fake_index.calls(), 1);
    }

    #[test]
    fn successful_seeded_dml_records_envelope_before_shard_dispatch() {
        let store = store_with_shards_and_property();
        let plan = seeded_dml_plan();
        let plan_blob = seeded_dml_bundle(&plan);
        let fake_index = FakeIndex::new(vec![Ok(vec![PostingHit {
            shard_id: ShardId::new(0),
            vertex_id: 42,
        }])]);

        let err = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            "client-key-1",
        ))
        .expect_err("native graph dispatch should fail after envelope");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(fake_index.calls(), 1);

        let record = store
            .router_mutation_record(&mutation_key_for(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
            ))
            .expect("mutation record");
        assert_eq!(record.as_v1().mutation_id, 1);
        assert!(!record.as_v1().routing_in_progress);
        assert!(record.as_v1().completed_row_count.is_none());
        assert_eq!(record.shards().len(), 1);
        assert_eq!(record.shards()[0].shard_id(), ShardId::new(0));
        assert_eq!(record.shards()[0].graph_canister(), graph_principal(1));
        assert!(!record.shards()[0].completed());

        let resolved = record
            .as_v1()
            .resolved_labels
            .as_ref()
            .expect("resolved labels");
        assert_eq!(resolved.vertex.len(), 1);
        assert_eq!(resolved.vertex[0].name, "Person");
        assert_eq!(resolved.vertex[0].id.raw(), 1);

        let seed_blob = record.shards()[0]
            .seed_bindings_blob()
            .as_ref()
            .expect("seed bindings");
        let seeds: SeedBindingsWire =
            candid::Decode!(seed_blob, SeedBindingsWire).expect("decode seeds");
        assert_eq!(seeds.entries.len(), 1);
        assert_eq!(seeds.entries[0].variable, "u");
        assert_eq!(seeds.entries[0].local_vertex_ids, vec![42]);
    }

    #[test]
    fn resolve_seed_routings_multi_fans_out_by_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            physical_index_id: PhysicalIndexId::new(1).expect("physical id"),
            payload_bytes: vec![1, 2, 3],
        };
        let hits = vec![
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 10,
            },
            PostingHit {
                shard_id: ShardId::new(1),
                vertex_id: 20,
            },
        ];
        let routings = resolve_seed_routings_multi(
            &store,
            SeedHits::Vertices(hits),
            tenant_main_graph_id(),
            IndexAnchor::Equal(probe),
        )
        .expect("route");
        assert_eq!(routings.len(), 2);
        assert_eq!(routings[0].shard_id, ShardId::new(0));
        assert_eq!(routings[1].shard_id, ShardId::new(1));
        let SeedHits::Vertices(shard_hits) = &routings[0].hits else {
            panic!("expected vertex hits");
        };
        assert_eq!(shard_hits.len(), 1);
        assert_eq!(shard_hits[0].vertex_id, 10);
        assert!(routings[0].anchor.is_some());
        assert_eq!(routings[0].graph_canister, graph_principal(1));
    }

    #[test]
    fn resolve_seed_routings_multi_fans_out_labeled_node_scan() {
        let store = store_with_shards();
        let anchor = IndexAnchor::Label {
            variable: "n".into(),
            vertex_label_id: 1,
        };
        let hits = vec![
            PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 10,
            },
            PostingHit {
                shard_id: ShardId::new(1),
                vertex_id: 20,
            },
        ];
        let routings = resolve_seed_routings_multi(
            &store,
            SeedHits::Vertices(hits),
            tenant_main_graph_id(),
            anchor,
        )
        .expect("route");
        assert_eq!(routings.len(), 2);
        let blob7 = routings[0]
            .anchor
            .as_ref()
            .and_then(|a| {
                let SeedHits::Vertices(shard_hits) = &routings[0].hits else {
                    return None;
                };
                seeds_for_local_shard(a.variable(), shard_hits, routings[0].shard_id)
            })
            .expect("shard 7 seeds");
        let seeds: SeedBindingsWire = candid::Decode!(&blob7, SeedBindingsWire).expect("decode");
        assert_eq!(seeds.entries[0].variable, "n");
        assert_eq!(seeds.entries[0].local_vertex_ids, vec![10]);
    }

    #[test]
    fn resolve_seed_routings_multi_discards_unknown_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            physical_index_id: PhysicalIndexId::new(1).expect("physical id"),
            payload_bytes: vec![],
        };
        let hits = vec![PostingHit {
            shard_id: ShardId::new(99),
            vertex_id: 1,
        }];
        let routings = resolve_seed_routings_multi(
            &store,
            SeedHits::Vertices(hits),
            tenant_main_graph_id(),
            IndexAnchor::Equal(probe),
        )
        .expect("stale index posting is discarded");
        assert!(routings.is_empty());
    }

    #[test]
    fn compound_label_and_property_seed_routing_intersects_hits() {
        let store = store_with_person_and_region_property();
        let plan = compound_label_property_read_plan();
        let stats =
            RouterGraphStats::test_vertex_indexed(tenant_main_graph_id(), &store, &["region"]);
        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("anchors")
        .expect("compound anchors");
        assert_eq!(set.anchors().len(), 2);

        let fake = compound_seed_fake_index();
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut _test_metrics = super::SeedResolutionMetrics::default();
        let hits = futures::executor::block_on(super::resolve_seed_hits_from_anchors(
            &fake,
            set.anchors(),
            &[ShardId::new(0), ShardId::new(1)],
            None,
            &mut _test_metrics,
        ))
        .expect("intersect label and property hits");
        assert_eq!(
            hits,
            SeedHits::Vertices(vec![PostingHit {
                shard_id: ShardId::new(0),
                vertex_id: 10,
            }])
        );

        let routings =
            resolve_seed_routings_multi(&store, hits, tenant_main_graph_id(), set.routing_anchor())
                .expect("route");
        let dispatches = routings_to_dispatches(routings);
        assert_eq!(dispatches.len(), 1);
        let seed_blob = dispatches[0]
            .seed_bindings_blob
            .as_ref()
            .expect("compound seeds");
        let seeds: SeedBindingsWire = Decode!(seed_blob, SeedBindingsWire).expect("decode seeds");
        assert_eq!(seeds.entries[0].variable, "n");
        assert_eq!(seeds.entries[0].local_vertex_ids, vec![10]);
    }

    #[test]
    fn compound_label_property_read_dispatch_intersects_index_and_label_export() {
        let store = store_with_person_and_region_property();
        let plan = compound_label_property_read_plan();
        let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode");
        let fake = compound_seed_fake_index();
        let shards = store
            .list_shards_for_graph_id(tenant_main_graph_id())
            .expect("shards");
        let stats =
            RouterGraphStats::test_vertex_indexed(tenant_main_graph_id(), &store, &["region"]);

        let err = futures::executor::block_on(dispatch_plan_blob_with_index(
            tenant_main_graph_id(),
            &plan_blob,
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &[],
            GqlExecutionMode::Query,
            None,
            &store,
            shards,
            &fake,
            Principal::anonymous(),
            &stats,
            None,
        ))
        .expect_err("native graph dispatch should fail after compound seeding");

        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(fake.equal_calls(), 1);
        assert!(fake.page_calls() >= 1);
    }

    #[test]
    fn label_intersection_seed_routing_fans_out_with_bindings() {
        let store = store_with_person_employee_labels();
        let plan = label_intersection_read_plan();
        let stats = tenant_main_stats();
        let set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &store,
            &stats,
        )
        .expect("anchors")
        .expect("label intersection anchors");
        assert!(matches!(
            set.routing_anchor(),
            IndexAnchor::LabelIntersection { .. }
        ));

        let fake = label_intersection_fake_index();
        let hits = futures::executor::block_on(collect_label_intersection_hits_for_shards(
            &fake,
            &[1, 2],
            &[ShardId::new(0), ShardId::new(1)],
        ))
        .expect("collect intersection hits");
        assert_eq!(
            hits,
            vec![
                PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 10,
                },
                PostingHit {
                    shard_id: ShardId::new(1),
                    vertex_id: 20,
                },
            ]
        );

        let routings = resolve_seed_routings_multi(
            &store,
            SeedHits::Vertices(hits),
            tenant_main_graph_id(),
            set.routing_anchor(),
        )
        .expect("route");
        let dispatches = routings_to_dispatches(routings);
        assert_eq!(dispatches.len(), 2);

        let seed_blob_7 = dispatches[0]
            .seed_bindings_blob
            .as_ref()
            .expect("shard 7 seeds");
        let seeds_7: SeedBindingsWire =
            Decode!(seed_blob_7, SeedBindingsWire).expect("decode shard 7 seeds");
        assert_eq!(seeds_7.entries[0].variable, "n");
        assert_eq!(seeds_7.entries[0].local_vertex_ids, vec![10]);

        let seed_blob_9 = dispatches[1]
            .seed_bindings_blob
            .as_ref()
            .expect("shard 9 seeds");
        let seeds_9: SeedBindingsWire =
            Decode!(seed_blob_9, SeedBindingsWire).expect("decode shard 9 seeds");
        assert_eq!(seeds_9.entries[0].local_vertex_ids, vec![20]);
    }

    #[test]
    fn label_intersection_read_dispatch_collects_label_export() {
        let store = store_with_person_employee_labels();
        let plan = label_intersection_read_plan();
        let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode");
        let fake = label_intersection_fake_index();
        let shards = store
            .list_shards_for_graph_id(tenant_main_graph_id())
            .expect("shards");

        let err = futures::executor::block_on(dispatch_plan_blob_with_index(
            tenant_main_graph_id(),
            &plan_blob,
            std::slice::from_ref(&plan),
            &BTreeMap::new(),
            &[],
            GqlExecutionMode::Query,
            None,
            &store,
            shards,
            &fake,
            Principal::anonymous(),
            &tenant_main_stats(),
            None,
        ))
        .expect_err("native graph dispatch should fail after index seeding");

        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(fake.page_calls(), 2);
        assert_eq!(fake.sieve_calls(), 2);
    }

    #[test]
    fn dispatch_plan_blob_decodes_for_multi_shard_seeded_read() {
        use gleaph_gql::ast::Expr;
        use gleaph_gql_planner::plan::ProjectColumn;
        use gleaph_gql_planner::wire::decode_plan_bundle;

        use crate::federation::federated_dispatch_plan_blob;

        let plan = PhysicalPlan::from_ops(vec![
            PlanOp::IndexScan {
                variable: Rc::from("u"),
                property: Rc::from("uid"),
                value: ScanValue::Literal(Value::Text("alice".into())),
                cmp: CmpOp::Eq,
                property_projection: None,
                ordered_by_sort: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::var("u"),
                    alias: Some(Rc::from("u")),
                }],
                distinct: false,
            },
        ]);
        let plan_blob = encode_block_plans(std::slice::from_ref(&plan), false).expect("encode");
        let dispatch =
            federated_dispatch_plan_blob(2, &plan_blob, std::slice::from_ref(&plan), false)
                .expect("dispatch");
        let (_, decoded) = decode_plan_bundle(&dispatch).expect("decode dispatch blob");
        assert_eq!(decoded.len(), 1);
        assert!(
            decoded[0]
                .ops
                .iter()
                .any(|op| matches!(op, PlanOp::IndexScan { .. }))
        );
    }

    #[test]
    fn label_stats_projection_gap_is_rejected() {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
            provision_canister: None,
        });
        let shard_id = ShardId::new(0);
        let graph = graph_principal(1);
        let deltas = vec![LabelStatsDeltaEventWire {
            mutation_id: 1,
            shard_event_seq: 2,
            label_stats_delta: LabelStatsDelta::default(),
        }];

        let err = futures::executor::block_on(store.advance_label_stats_projection(
            GraphId::from_raw(0),
            graph,
            shard_id,
            10,
            |_graph, _from_seq, _limit| async { Ok(deltas) },
            |_graph, _through_seq| async { Ok(()) },
        ))
        .expect_err("gap should fail");

        assert!(matches!(err, RouterError::InvalidArgument(_)));
        assert_eq!(
            store.label_stats_projection_cursor(GraphId::from_raw(0), shard_id),
            0
        );
    }

    // ADR 0029 §5 (Phase 3) read barrier decision logic. Production uses the real
    // inter-canister pending-mutation lookup; the private `enforce_read_consistency_with_lookup`
    // seam makes the graph-index branch host-testable. These host tests pin every local
    // decision: `Eventual` no-op, label-stats short-circuit,
    // graph-index lag and drain, and multi-shard lookup identity/order.
    #[test]
    fn read_barrier_eventual_is_noop() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        futures::executor::block_on(enforce_read_consistency(
            &store,
            graph_id,
            &ReadMode::Eventual,
        ))
        .expect("eventual never blocks");
    }

    #[test]
    fn read_barrier_atleast_label_stats_lag_returns_retryable_projection_lag() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        // Cursor defaults to 0; require seq 5 on shard 0 → unmet, so the barrier must
        // return ProjectionLag *before* any inter-canister index call. The injected
        // lookup panics if called, tightening the short-circuit to zero index calls.
        let token = MutationToken {
            mutation_id: 1,
            shards: vec![MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: Some(5),
            }],
        };
        let err = futures::executor::block_on(enforce_read_consistency_with_lookup(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
            |_graph| async { panic!("index lookup must not be called for label-stats lag") },
        ))
        .expect_err("label stats projection has not caught up");
        match err {
            RouterError::ProjectionLag {
                shard_id,
                watermark,
                required,
                current,
            } => {
                assert_eq!(shard_id, 0);
                assert_eq!(watermark, "label_stats");
                assert_eq!(required, 5);
                assert_eq!(current, 0);
            }
            other => panic!("expected ProjectionLag, got {other:?}"),
        }
    }

    #[test]
    fn read_barrier_atleast_empty_token_is_satisfied() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        let token = MutationToken {
            mutation_id: 7,
            shards: vec![],
        };
        futures::executor::block_on(enforce_read_consistency(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
        ))
        .expect("no shard watermarks to satisfy");
    }

    #[test]
    fn read_barrier_atleast_non_empty_satisfied_token_is_admitted() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        // A non-empty token whose label-stats cursor is already caught up and whose
        // graph-index watermark is past the mutation id must be admitted.
        let token = MutationToken {
            mutation_id: 5,
            shards: vec![MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: Some(0),
            }],
        };
        futures::executor::block_on(enforce_read_consistency_with_lookup(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
            |_graph| async { Ok::<_, String>(Some(10)) },
        ))
        .expect("non-empty satisfied token should be admitted");
    }

    #[test]
    fn read_barrier_atleast_graph_index_lag_returns_exact_projection_lag() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        let token = MutationToken {
            mutation_id: 7,
            shards: vec![MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: None,
            }],
        };
        let err = futures::executor::block_on(enforce_read_consistency_with_lookup(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
            |_graph| async { Ok::<_, String>(Some(7)) },
        ))
        .expect_err("graph-index watermark is not drained");
        match err {
            RouterError::ProjectionLag {
                shard_id,
                watermark,
                required,
                current,
            } => {
                assert_eq!(shard_id, 0);
                assert_eq!(watermark, "graph_index");
                assert_eq!(required, 7);
                assert_eq!(current, 7);
            }
            other => panic!("expected ProjectionLag, got {other:?}"),
        }
    }

    #[test]
    fn read_barrier_atleast_none_index_watermark_is_satisfied() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        let token = MutationToken {
            mutation_id: 3,
            shards: vec![MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: None,
            }],
        };
        futures::executor::block_on(enforce_read_consistency_with_lookup(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
            |_graph| async { Ok::<_, String>(None) },
        ))
        .expect("drained index watermark should satisfy any mutation_id");
    }

    #[test]
    fn read_barrier_atleast_multishard_lookup_identity_and_order() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        let token = MutationToken {
            mutation_id: 1,
            shards: vec![
                MutationTokenShard {
                    shard_id: ShardId::new(0),
                    label_stats_seq: None,
                },
                MutationTokenShard {
                    shard_id: ShardId::new(1),
                    label_stats_seq: None,
                },
            ],
        };
        let mut calls: Vec<Principal> = Vec::new();
        futures::executor::block_on(enforce_read_consistency_with_lookup(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
            |graph| {
                calls.push(graph);
                async { Ok::<_, String>(Some(5)) }
            },
        ))
        .expect("all shard watermarks satisfied");
        assert_eq!(calls, vec![graph_principal(1), graph_principal(4)]);
    }

    /// ADR 0030 slice 5a: the single-shard admission gate must run *before* the saga envelope record
    /// and Try, so a constrained insert that resolves to more than one shard is refused with no
    /// reservation residue. The only way to reach the gate with >1 dispatch is a pre-recorded
    /// multi-shard saga envelope (fresh routing of a pure insert always lands on one shard), so the
    /// test seeds that envelope and drives the real dispatch path.
    #[test]
    fn single_shard_gate_rejects_constrained_multishard_dispatch_without_reservation_residue() {
        use crate::facade::stable::constraint_name_catalog::lookup_constraint_name_id;
        use crate::facade::stable::label_stats::RouterMutationShardV1;
        use crate::facade::stable::reservation_catalog::{self, ProofShard, ReservationClaim};
        use crate::facade::store::ClientMutationReservation;
        use gleaph_gql::ast::{Expr, ExprKind};
        use gleaph_gql_ic::{UniqueKeyOutcome, encode_unique_value};
        use gleaph_gql_planner::plan::PropertyAssignment;
        use gleaph_graph_kernel::federation::ShardId;
        use gleaph_graph_kernel::plan_exec::{
            ResolvedLabelTable, ResolvedProperty, ResolvedPropertyTable, ResolvedVertexLabel,
        };

        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        let caller = Principal::anonymous();
        let key = "ck-multishard";

        store
            .create_unique_constraint(graph_id, "user_email", false, "User", "email")
            .expect("declare constraint");
        let label_id = store
            .lookup_vertex_label_id(graph_id, "User")
            .expect("User interned");
        let property_id = store
            .lookup_property_id(graph_id, "email")
            .expect("email interned");

        let plan = PhysicalPlan::from_ops(vec![PlanOp::InsertVertex {
            variable: Some(Rc::from("n")),
            labels: vec![NodeLabelRef::from("User")],
            properties: vec![PropertyAssignment {
                name: "email".into(),
                value: Expr::new(ExprKind::Literal(Value::Text("a@x".into()))),
            }],
        }]);
        let plan_blob = seeded_dml_bundle(&plan);
        let fingerprint = request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update);

        // Seed a 2-shard saga envelope under this key so dispatch resolves to two dispatches.
        let reservation: ClientMutationReservation = store
            .reserve_mutation_id_for_client_key(caller, graph_id, key, fingerprint)
            .expect("reserve mutation id");
        assert!(reservation.routing_owner);
        let resolved_labels = ResolvedLabelTable {
            vertex: vec![ResolvedVertexLabel {
                name: "User".into(),
                id: label_id,
            }],
            edge: vec![],
        };
        let resolved_properties = ResolvedPropertyTable {
            properties: vec![ResolvedProperty {
                name: "email".into(),
                id: property_id,
            }],
        };
        store
            .record_router_mutation_shards(
                &mutation_key_for(caller, graph_id, key),
                resolved_labels,
                resolved_properties,
                vec![
                    RouterMutationShardV1::new(ShardId::new(0), graph_principal(1), None),
                    RouterMutationShardV1::new(ShardId::new(1), graph_principal(4), None),
                ],
            )
            .expect("record 2-shard envelope");

        let fake_index = FakeIndex::new(vec![]);
        let err = futures::executor::block_on(dispatch_with_fake_index(
            &store,
            &fake_index,
            &plan,
            &plan_blob,
            key,
        ))
        .expect_err("constrained insert fanning out to 2 shards must be refused");
        assert!(matches!(err, RouterError::NotImplemented(_)), "got {err:?}");
        // The gate ran before any index work.
        assert_eq!(fake_index.calls(), 0);

        // No reservation residue: a fresh mutation can reserve the same value (a stranded `Reserved`
        // record would have returned `UniquenessReservationInFlight` here).
        let constraint_id =
            lookup_constraint_name_id(graph_id, "user_email").expect("constraint id");
        let encoded = match encode_unique_value(&Value::Text("a@x".into())) {
            UniqueKeyOutcome::Claim(bytes) => bytes,
            other => panic!("expected a claim, got {other:?}"),
        };
        reservation_catalog::try_reserve(
            graph_id,
            999,
            &[ReservationClaim {
                constraint_id,
                encoded_value: encoded,
                claim_ordinal: 0,
            }],
            &[ProofShard::new(ShardId::new(0), graph_principal(1))],
            1,
        )
        .expect("no residual reservation fences a later insert of the same value");
    }

    /// ADR 0030 slice 5a Confirm orchestration (`confirm_proofs_collect_acks`): the two safety
    /// contracts that `confirm_reservation`'s own unit tests do not cover at the orchestration layer.
    mod confirm_orchestration {
        use super::super::confirm_proofs_collect_acks;
        use crate::facade::stable::label_stats::ClientMutationKey;
        use crate::facade::stable::reservation_catalog::{self, ProofShard, ReservationClaim};
        use crate::facade::store::RouterStore;
        use candid::Principal;
        use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};
        use gleaph_graph_kernel::federation::{
            ClaimId, EffectId, ShardId, UniqueAcquireEvidence, UniqueAcquireProof,
        };
        use gleaph_graph_kernel::plan_exec::UniqueClaimDispatch;

        const CONSTRAINT: u16 = 5;

        fn graph(seed: u32) -> GraphId {
            GraphId::from_raw(910_000 + seed)
        }

        fn claims() -> Vec<UniqueClaimDispatch> {
            vec![UniqueClaimDispatch {
                claim_ordinal: 0,
                constraint_id: ConstraintNameId::from_raw(CONSTRAINT),
                encoded_value: b"v".to_vec(),
            }]
        }

        fn seed_reserved(store: &RouterStore, g: GraphId, mutation_id: u64) {
            reservation_catalog::try_reserve(
                g,
                mutation_id,
                &[ReservationClaim {
                    constraint_id: ConstraintNameId::from_raw(CONSTRAINT),
                    encoded_value: b"v".to_vec(),
                    claim_ordinal: 0,
                }],
                &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
                1,
            )
            .expect("seed reserved");
            // Mirror the production Try: a fresh reservation also bumps the reverse-index count that
            // a `FreshlyCommitted` Confirm decrements (ADR 0030 slice 6). A constant key keeps the
            // `mutation_id → ClientMutationKey` mapping consistent across these single-claim seeds.
            store.apply_reservation_slots(mutation_id, &seed_key(), 1);
        }

        fn seed_key() -> ClientMutationKey {
            ClientMutationKey::new(
                Principal::anonymous(),
                GraphId::from_raw(0),
                "confirm-orch".into(),
            )
        }

        fn proof(claim_id: ClaimId, acquire: Option<UniqueAcquireEvidence>) -> UniqueAcquireProof {
            UniqueAcquireProof { claim_id, acquire }
        }

        /// Project the `(effect_id, claim)` pairs down to the acked effect ids.
        fn effect_ids(acks: &[(EffectId, &UniqueClaimDispatch)]) -> Vec<EffectId> {
            acks.iter().map(|(effect, _)| *effect).collect()
        }

        fn evidence() -> UniqueAcquireEvidence {
            UniqueAcquireEvidence {
                effect_id: EffectId::new(10, 0),
                owner_element_id: vec![7u8; 8],
            }
        }

        /// A later `try_reserve` of the same value distinguishes the reservation's state without a
        /// catalog peek: `Ok` ⇒ gone, `UniquenessViolation` ⇒ Committed, `…InFlight` ⇒ still Reserved.
        fn second_try(g: GraphId) -> Result<(), crate::state::RouterError> {
            reservation_catalog::try_reserve(
                g,
                20,
                &[ReservationClaim {
                    constraint_id: ConstraintNameId::from_raw(CONSTRAINT),
                    encoded_value: b"v".to_vec(),
                    claim_ordinal: 0,
                }],
                &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
                2,
            )
            .map(|_| ())
        }

        #[test]
        fn matching_proof_confirms_and_returns_effect_for_ack() {
            let store = RouterStore::new();
            let g = graph(1);
            seed_reserved(&store, g, 10);

            let dispatches = claims();
            let acks = confirm_proofs_collect_acks(
                &store,
                g,
                10,
                &dispatches,
                vec![proof(ClaimId::new(10, 0), Some(evidence()))],
            );
            assert_eq!(effect_ids(&acks), vec![EffectId::new(10, 0)]);
            // Reservation is now Committed.
            assert!(matches!(
                second_try(g),
                Err(crate::state::RouterError::UniquenessViolation(_))
            ));
        }

        #[test]
        fn absent_acquire_is_not_acked_and_leaves_reservation_reserved() {
            let store = RouterStore::new();
            let g = graph(2);
            seed_reserved(&store, g, 10);

            let dispatches = claims();
            let acks = confirm_proofs_collect_acks(
                &store,
                g,
                10,
                &dispatches,
                vec![proof(ClaimId::new(10, 0), None)],
            );
            assert!(acks.is_empty(), "a non-commit proof must not ack");
            assert!(matches!(
                second_try(g),
                Err(crate::state::RouterError::UniquenessReservationInFlight(_))
            ));
        }

        #[test]
        fn full_claim_id_mismatch_is_not_acked() {
            let store = RouterStore::new();
            let g = graph(3);
            seed_reserved(&store, g, 10);

            // Same ordinal, different mutation: the full ClaimId does not match, so the proof is for
            // a different mutation's claim and must be ignored.
            let dispatches = claims();
            let acks = confirm_proofs_collect_acks(
                &store,
                g,
                10,
                &dispatches,
                vec![proof(ClaimId::new(999, 0), Some(evidence()))],
            );
            assert!(acks.is_empty(), "a foreign ClaimId must not ack");
            assert!(matches!(
                second_try(g),
                Err(crate::state::RouterError::UniquenessReservationInFlight(_))
            ));
        }

        #[test]
        fn confirm_returning_false_does_not_ack() {
            // No reservation exists for the value, so `confirm_unique_claim` returns false; the
            // effect must NOT be acked (acking would destroy the only commit evidence).
            let store = RouterStore::new();
            let g = graph(4);

            let dispatches = claims();
            let acks = confirm_proofs_collect_acks(
                &store,
                g,
                10,
                &dispatches,
                vec![proof(ClaimId::new(10, 0), Some(evidence()))],
            );
            assert!(
                acks.is_empty(),
                "a failed Reserved→Committed transition must not ack"
            );
        }

        #[test]
        fn already_committed_claim_re_acks_idempotently() {
            // Confirm is idempotent: a replay against an already-`Committed` claim (e.g. the first
            // Confirm committed but its ack call failed) must report the effect for ack again so the
            // pinned `Acquire` is eventually unpinned.
            let store = RouterStore::new();
            let g = graph(5);
            seed_reserved(&store, g, 10);

            let dispatches = claims();
            let first = confirm_proofs_collect_acks(
                &store,
                g,
                10,
                &dispatches,
                vec![proof(ClaimId::new(10, 0), Some(evidence()))],
            );
            assert_eq!(effect_ids(&first), vec![EffectId::new(10, 0)]);

            // Reservation is now Committed; a replayed Confirm of the same claim re-acks.
            let replay = confirm_proofs_collect_acks(
                &store,
                g,
                10,
                &dispatches,
                vec![proof(ClaimId::new(10, 0), Some(evidence()))],
            );
            assert_eq!(
                effect_ids(&replay),
                vec![EffectId::new(10, 0)],
                "idempotent re-confirm must re-ack so a previously-failed ack is retried"
            );
        }
    }

    /// ADR 0030 slice 5b Release orchestration (`reconcile_releases_collect_acks`): an effect is
    /// acked only when the value is durably free for this owner; a held release (Release-before-
    /// Acquire) is left pinned for slice-6 recovery.
    mod release_orchestration {
        use super::super::reconcile_releases_collect_acks;
        use crate::facade::stable::reservation_catalog::{self, ProofShard, ReservationClaim};
        use crate::facade::store::RouterStore;
        use candid::Principal;
        use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};
        use gleaph_graph_kernel::federation::{
            ClaimId, EffectId, ShardId, UniqueEffectOp, UniqueEffectReceipt,
        };

        const CONSTRAINT: u16 = 6;

        fn graph(seed: u32) -> GraphId {
            GraphId::from_raw(920_000 + seed)
        }

        fn cid() -> ConstraintNameId {
            ConstraintNameId::from_raw(CONSTRAINT)
        }

        fn reserve(g: GraphId) {
            reservation_catalog::try_reserve(
                g,
                10,
                &[ReservationClaim {
                    constraint_id: cid(),
                    encoded_value: b"v".to_vec(),
                    claim_ordinal: 0,
                }],
                &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
                1,
            )
            .expect("reserve");
        }

        fn commit(g: GraphId, owner: &[u8]) {
            reserve(g);
            assert_eq!(
                reservation_catalog::confirm_reservation(
                    g,
                    ClaimId::new(10, 0),
                    cid(),
                    b"v",
                    owner.to_vec(),
                    EffectId::new(10, 0)
                ),
                reservation_catalog::ConfirmOutcome::FreshlyCommitted
            );
            // Model the steady state a `Release` reconciles against: the `Acquire` was already
            // acked/unpinned, so the owner-matched Release removes the reservation rather than
            // holding to protect a still-pinned Acquire.
            assert!(reservation_catalog::clear_acquire_ack(
                g,
                cid(),
                b"v",
                ClaimId::new(10, 0)
            ));
        }

        fn release_effect(effect_ordinal: u32, owner: &[u8]) -> UniqueEffectReceipt {
            UniqueEffectReceipt {
                effect_id: EffectId::new(20, effect_ordinal),
                claim_id: None,
                owner_element_id: owner.to_vec(),
                constraint_id: cid(),
                encoded_value: b"v".to_vec(),
                op: UniqueEffectOp::Release,
            }
        }

        /// `Ok` ⇒ the value is free (reservation removed), `Err` ⇒ a reservation still holds it.
        fn value_is_free(g: GraphId) -> bool {
            reservation_catalog::try_reserve(
                g,
                999,
                &[ReservationClaim {
                    constraint_id: cid(),
                    encoded_value: b"v".to_vec(),
                    claim_ordinal: 0,
                }],
                &[ProofShard::new(ShardId::new(0), Principal::anonymous())],
                2,
            )
            .is_ok()
        }

        #[test]
        fn owner_matched_release_removes_reservation_and_acks() {
            let store = RouterStore::new();
            let g = graph(1);
            let owner = vec![7u8; 8];
            commit(g, &owner);

            let acks = reconcile_releases_collect_acks(&store, g, vec![release_effect(0, &owner)]);
            assert_eq!(acks, vec![EffectId::new(20, 0)]);
            assert!(value_is_free(g), "owner-matched release frees the value");
        }

        #[test]
        fn held_release_is_not_acked() {
            // The value's Acquire is still Reserved (owner undetermined) → Release-before-Acquire
            // hold: not acked, reservation untouched.
            let store = RouterStore::new();
            let g = graph(2);
            reserve(g);

            let acks =
                reconcile_releases_collect_acks(&store, g, vec![release_effect(0, &[7u8; 8])]);
            assert!(acks.is_empty(), "a held release must not be acked");
            assert!(
                !value_is_free(g),
                "held release leaves the reservation in place"
            );
        }

        #[test]
        fn release_on_missing_reservation_is_acked_noop() {
            // Nothing reserved (already released) → re-ack-safe no-op.
            let store = RouterStore::new();
            let g = graph(3);

            let acks =
                reconcile_releases_collect_acks(&store, g, vec![release_effect(0, &[7u8; 8])]);
            assert_eq!(acks, vec![EffectId::new(20, 0)]);
        }

        #[test]
        fn stale_release_with_different_owner_is_acked_and_keeps_live_reservation() {
            // A different element took the value over; the old element's Release is stale → no-op ack,
            // and the live reservation must survive.
            let store = RouterStore::new();
            let g = graph(4);
            let live_owner = vec![9u8; 8];
            commit(g, &live_owner);

            let acks =
                reconcile_releases_collect_acks(&store, g, vec![release_effect(0, &[1u8; 8])]);
            assert_eq!(
                acks,
                vec![EffectId::new(20, 0)],
                "stale release is ack-able"
            );
            assert!(
                !value_is_free(g),
                "stale release must not remove the live reservation"
            );
        }
    }

    #[test]
    fn wave_4_multi_anchor_seed_extracts_two_variables() {
        use crate::gql::build_router_block_plan;
        use crate::planner_stats::RouterGraphStats;
        use gleaph_gql::type_check::NoSchema;
        use std::collections::BTreeMap;

        let store = store_with_one_shard();
        let graph_id = tenant_main_graph_id();
        let admin = Principal::from_slice(&[1; 29]);
        store
            .admin_intern_vertex_label(admin, "tenant.main", "Post")
            .expect("intern Post");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.main",
            "demo_id",
        );
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            admin,
            "tenant.main",
            "demo_graph",
        );

        let gql = "MATCH (a:Post {demo_id: $a_demo_id, demo_graph: 'social'}), (b:Post {demo_id: $b_demo_id, demo_graph: 'social'}) RETURN a NEXT INSERT (a)-[:REPLY_TO {demo_edge_id: 'r', demo_kind: 'reply'}]->(b)";
        let program = parser::parse(gql).expect("parse");
        let block = program
            .transaction_activity
            .as_ref()
            .unwrap()
            .body
            .as_ref()
            .unwrap();

        let stats_no_index = RouterGraphStats::from_catalog(
            graph_id,
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        );
        let plan_no_index =
            build_router_block_plan(block, &NoSchema, &stats_no_index).expect("plan no index");
        // Plan 0311: on a single-shard topology a multi-variable label-only prefix KEEPS its
        // per-variable anchors — label postings seed the routing variable and the sole shard
        // executes the remaining labeled scans locally once the wire guard sees effective
        // seeds. Multi-shard stores still get None (pinned by
        // from_plans_multi_shard_still_drops_label_only_multi_variable_prefix).
        let label_only_set =
            SeedAnchorSet::from_plans(&[plan_no_index], &BTreeMap::new(), &store, &stats_no_index)
                .expect("parse anchors")
                .expect("standalone keeps label-only multi-variable anchors");
        assert_eq!(
            label_only_set
                .variables
                .iter()
                .map(|v| v.variable.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "both labeled endpoints anchor"
        );
        for variable in &label_only_set.variables {
            assert!(
                variable
                    .anchors
                    .iter()
                    .all(|a| matches!(a, IndexAnchor::Label { .. })),
                "no-index extraction yields plain label anchors only"
            );
        }

        crate::facade::store::catalog_test_support::register_active_vertex_index(
            &store, graph_id, 0, "demo_id",
        );
        let stats_indexed = RouterGraphStats::test_vertex_indexed(graph_id, &store, &["demo_id"]);
        let plan_indexed =
            build_router_block_plan(block, &NoSchema, &stats_indexed).expect("plan indexed");
        let mut params = BTreeMap::new();
        params.insert("$a_demo_id".to_string(), gleaph_gql::Value::Uint64(1));
        params.insert("$b_demo_id".to_string(), gleaph_gql::Value::Uint64(2));
        let indexed_set = SeedAnchorSet::from_plans(
            std::slice::from_ref(&plan_indexed),
            &params,
            &store,
            &stats_indexed,
        )
        .expect("parse anchors")
        .expect("multi-variable indexed plan must produce seed anchors");
        assert_eq!(
            indexed_set
                .variables
                .iter()
                .map(|v| v.variable.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"],
            "indexed demo_id should anchor both a and b"
        );
    }

    /// Records every `lookup_range` request so tests can assert the exact pushed-down shape.
    #[derive(Default)]
    struct RangeRecordingIndex {
        ranges:
            std::rc::Rc<std::cell::RefCell<Vec<gleaph_graph_kernel::index::PostingRangeRequest>>>,
        edge_ranges: std::rc::Rc<
            std::cell::RefCell<
                Vec<(
                    u32,
                    gleaph_graph_kernel::index::PostingRangeRequest,
                    Option<u16>,
                )>,
            >,
        >,
        edge_hits: std::rc::Rc<std::cell::RefCell<Vec<gleaph_graph_kernel::index::EdgePostingHit>>>,
    }

    impl RangeRecordingIndex {
        fn record_edge_hit(&self, hit: gleaph_graph_kernel::index::EdgePostingHit) {
            self.edge_hits.borrow_mut().push(hit);
        }
    }

    impl IndexLookup for RangeRecordingIndex {
        fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            range: gleaph_graph_kernel::index::PostingRangeRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.ranges.borrow_mut().push(range);
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_intersection(
            &self,
            _req: gleaph_graph_kernel::index::IndexIntersectionRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            gleaph_graph_kernel::index::IndexIntersectionResult,
                            String,
                        >,
                    > + '_,
            >,
        > {
            Box::pin(async move {
                Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(Vec::new()))
            })
        }

        fn lookup_edge_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_edge_range(
            &self,
            physical_index_id: PhysicalIndexId,
            property_id: u32,
            range: gleaph_graph_kernel::index::PostingRangeRequest,
            label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            let _ = physical_index_id;
            self.edge_ranges
                .borrow_mut()
                .push((property_id, range, label_id));
            let hits = self.edge_hits.borrow().clone();
            Box::pin(async move { Ok(hits) })
        }

        fn count_postings_by_value(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _min_count: u64,
            _vertex_filter_packed: Option<Vec<u64>>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn lookup_label_page(
            &self,
            _req: gleaph_graph_kernel::index::LabelLookupPageRequest,
        ) -> Pin<Box<dyn Future<Output = Result<LabelLookupPageResult, String>> + '_>> {
            Box::pin(async move {
                Ok(LabelLookupPageResult {
                    hits: Vec::new(),
                    next: None,
                    done: true,
                })
            })
        }

        fn lookup_label_intersection(
            &self,
            _req: IndexLabelIntersectionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn filter_hits_by_label(
            &self,
            _label_id: u32,
            hits: Vec<PostingHit>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(hits) })
        }

        fn count_postings_by_value_for_label(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _vertex_label_id: u32,
            _min_count: u64,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<ValuePostingCount>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }
    }

    fn range_probe_anchor(
        bound_value: Value,
        bound: crate::seed::SeedRangeBound,
    ) -> crate::seed::IndexAnchor {
        crate::seed::IndexAnchor::Range(crate::seed::RangeSeedProbe {
            variable: "n".to_owned(),
            property: "p".to_owned(),
            property_id: 11,
            physical_index_id: PhysicalIndexId::new(3).expect("physical id"),
            bound_bytes: gleaph_gql::value_to_index_key_bytes(&bound_value)
                .unwrap()
                .unwrap(),
            bound,
        })
    }

    #[test]
    fn range_seed_probe_executes_domain_clamped_between_requests() {
        let cases: Vec<(Value, crate::seed::SeedRangeBound, Vec<u8>, Vec<u8>)> = vec![
            (
                Value::Int64(5),
                crate::seed::SeedRangeBound::Ge,
                gleaph_gql::value_to_index_key_bytes(&Value::Int64(5))
                    .unwrap()
                    .unwrap(),
                // NUMERIC domain ceiling (tag 2 + 1).
                vec![3],
            ),
            (
                Value::Text("b".to_owned()),
                crate::seed::SeedRangeBound::Ge,
                gleaph_gql::value_to_index_key_bytes(&Value::Text("b".into()))
                    .unwrap()
                    .unwrap(),
                // TEXT domain ceiling (tag 6 + 1).
                vec![7],
            ),
            (
                Value::DateTime(100, 0),
                crate::seed::SeedRangeBound::Lt,
                // TEMPORAL DateTime subtype floor.
                vec![8, 4],
                gleaph_gql::value_to_index_key_bytes(&Value::DateTime(100, 0))
                    .unwrap()
                    .unwrap(),
            ),
        ];
        for (bound_value, bound, expected_low, expected_high) in cases {
            let index = RangeRecordingIndex::default();
            let anchor = range_probe_anchor(bound_value.clone(), bound);
            #[cfg_attr(
                not(feature = "batch-instr-log"),
                allow(clippy::default_constructed_unit_structs)
            )]
            let mut metrics = super::SeedResolutionMetrics::default();
            let hits = futures::executor::block_on(super::lookup_anchor_hits(
                &index,
                &anchor,
                &[],
                &mut metrics,
            ))
            .expect("probe lookup");
            assert!(matches!(&hits, SeedHits::Vertices(_)));
            let ranges = index.ranges.borrow();
            assert_eq!(
                ranges.as_slice(),
                &[gleaph_graph_kernel::index::PostingRangeRequest::Between {
                    low: expected_low,
                    high: expected_high,
                }],
                "{bound_value:?} {bound:?}"
            );
        }
    }

    #[test]
    fn range_seed_probe_empty_clamped_interval_issues_no_lookup() {
        // Nothing is strictly greater than the maximum DateTime: the clamped interval is
        // empty, so no index call is made at all.
        let index = RangeRecordingIndex::default();
        let anchor = range_probe_anchor(
            Value::DateTime(i64::MAX, u32::MAX),
            crate::seed::SeedRangeBound::Gt,
        );
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut metrics = super::SeedResolutionMetrics::default();
        let hits = futures::executor::block_on(super::lookup_anchor_hits(
            &index,
            &anchor,
            &[],
            &mut metrics,
        ))
        .expect("probe lookup");
        assert!(matches!(&hits, SeedHits::Vertices(v) if v.is_empty()));
        assert!(index.ranges.borrow().is_empty());
    }

    #[test]
    fn prefix_seed_probe_executes_text_prefix_between_request() {
        // The stored pattern key [6] + 'ab' + terminator lowers to the half-open
        // interval [[6,'a','b'], [6,'a','b',0xFF]); no empty-clamp case exists.
        let index = RangeRecordingIndex::default();
        let anchor = crate::seed::IndexAnchor::Prefix(crate::seed::PrefixSeedProbe {
            variable: "n".to_owned(),
            property: "p".to_owned(),
            property_id: 11,
            physical_index_id: PhysicalIndexId::new(3).expect("physical id"),
            pattern_bytes: gleaph_gql::value_to_index_key_bytes(&Value::Text("ab".into()))
                .unwrap()
                .unwrap(),
        });
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut metrics = super::SeedResolutionMetrics::default();
        let hits = futures::executor::block_on(super::lookup_anchor_hits(
            &index,
            &anchor,
            &[],
            &mut metrics,
        ))
        .expect("prefix probe lookup");
        assert!(matches!(&hits, SeedHits::Vertices(_)));
        assert_eq!(
            index.ranges.borrow().as_slice(),
            &[gleaph_graph_kernel::index::PostingRangeRequest::Between {
                low: vec![6, b'a', b'b'],
                high: vec![6, b'a', b'b', 255],
            }],
            "prefix probe must issue one TEXT prefix Between request"
        );
    }

    #[test]
    fn edge_range_seed_probe_executes_domain_clamped_between_per_wire_label() {
        use gleaph_graph_kernel::index::EdgePostingHit;

        let index = RangeRecordingIndex::default();
        // One distinct hit per wire label proves the sieve fans out; the duplicate proves the
        // `(shard, owner, label, slot)` dedupe.
        for (owner, slot) in [(7u32, 1u32), (8, 0), (7, 1)] {
            index.record_edge_hit(EdgePostingHit {
                shard_id: ShardId::new(0),
                owner_vertex_id: owner,
                label_id: if owner == 8 { 0x8002 } else { 0x8001 },
                slot_index: slot,
            });
        }
        let bound_bytes = gleaph_gql::value_to_index_key_bytes(&Value::Int64(5))
            .unwrap()
            .unwrap();
        let anchor = crate::seed::IndexAnchor::EdgeRange(crate::seed::EdgeRangeSeedProbe {
            variable: "e".into(),
            property: "weight".into(),
            property_id: 9,
            physical_index_id: PhysicalIndexId::new(3).unwrap(),
            bound_bytes: bound_bytes.clone(),
            bound: crate::seed::SeedRangeBound::Gt,
            wire_label_ids: vec![0x8001, 0x8002],
        });
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut metrics = super::SeedResolutionMetrics::default();
        let hits = futures::executor::block_on(super::lookup_anchor_hits(
            &index,
            &anchor,
            &[],
            &mut metrics,
        ))
        .expect("edge range probe lookup");
        let SeedHits::Edges(edges) = &hits else {
            panic!("expected edge hits, got {hits:?}");
        };
        assert_eq!(
            edges.len(),
            2,
            "duplicate posting across wire labels must dedupe"
        );

        let recorded = index.edge_ranges.borrow();
        assert_eq!(
            recorded.len(),
            2,
            "one clamped Between request per wire label"
        );
        for (property_id, request, label) in recorded.iter() {
            assert_eq!(*property_id, 9);
            let gleaph_graph_kernel::index::PostingRangeRequest::Between { low, high } = request
            else {
                panic!("edge range probe must issue a clamped Between, got {request:?}");
            };
            // Strictly-above 5 starts past the bound; the NUMERIC domain ceiling is tag 2 + 1.
            assert!(
                low.as_slice() > bound_bytes.as_slice(),
                "low must exclude the strict bound"
            );
            assert_eq!(high, &vec![3], "high must clamp to the NUMERIC ceiling");
            assert!(
                *label == Some(0x8001) || *label == Some(0x8002),
                "wire label sieve must reach the index call, got {label:?}"
            );
        }
    }

    #[test]
    fn edge_range_seed_probe_empty_clamped_interval_issues_no_lookup() {
        let index = RangeRecordingIndex::default();
        let anchor = crate::seed::IndexAnchor::EdgeRange(crate::seed::EdgeRangeSeedProbe {
            variable: "e".into(),
            property: "seen_at".into(),
            property_id: 4,
            physical_index_id: PhysicalIndexId::new(1).unwrap(),
            bound_bytes: gleaph_gql::value_to_index_key_bytes(&Value::DateTime(i64::MAX, u32::MAX))
                .unwrap()
                .unwrap(),
            bound: crate::seed::SeedRangeBound::Gt,
            wire_label_ids: Vec::new(),
        });
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut metrics = super::SeedResolutionMetrics::default();
        let hits = futures::executor::block_on(super::lookup_anchor_hits(
            &index,
            &anchor,
            &[],
            &mut metrics,
        ))
        .expect("edge range probe lookup");
        assert!(matches!(&hits, SeedHits::Edges(v) if v.is_empty()));
        assert!(index.edge_ranges.borrow().is_empty());
    }

    #[test]
    fn edge_prefix_seed_probe_executes_text_prefix_between_per_wire_label() {
        use gleaph_graph_kernel::index::EdgePostingHit;

        let index = RangeRecordingIndex::default();
        index.record_edge_hit(EdgePostingHit {
            shard_id: ShardId::new(0),
            owner_vertex_id: 5,
            label_id: 0x8001,
            slot_index: 3,
        });
        let pattern_bytes = gleaph_gql::value_to_index_key_bytes(&Value::Text("Str".into()))
            .unwrap()
            .unwrap();
        let anchor = crate::seed::IndexAnchor::EdgePrefix(crate::seed::EdgePrefixSeedProbe {
            variable: "e".into(),
            property: "name".into(),
            property_id: 9,
            physical_index_id: PhysicalIndexId::new(3).unwrap(),
            pattern_bytes: pattern_bytes.clone(),
            wire_label_ids: vec![0x8001],
        });
        #[cfg_attr(
            not(feature = "batch-instr-log"),
            allow(clippy::default_constructed_unit_structs)
        )]
        let mut metrics = super::SeedResolutionMetrics::default();
        let hits = futures::executor::block_on(super::lookup_anchor_hits(
            &index,
            &anchor,
            &[],
            &mut metrics,
        ))
        .expect("edge prefix probe lookup");
        let SeedHits::Edges(edges) = &hits else {
            panic!("expected edge hits, got {hits:?}");
        };
        assert_eq!(edges.len(), 1);

        let recorded = index.edge_ranges.borrow();
        assert_eq!(recorded.len(), 1);
        let (property_id, request, label) = &recorded[0];
        assert_eq!(*property_id, 9);
        assert_eq!(*label, Some(0x8001));
        // The seeded interval is the half-open encoded TEXT prefix window derived
        // from the stored key: [tag + payload, tag + payload + sentinel].
        let gleaph_graph_kernel::index::PostingRangeRequest::Between { low, high } = request else {
            panic!("edge prefix probe must issue a Between request, got {request:?}");
        };
        assert_eq!(
            low.as_slice(),
            &pattern_bytes[..pattern_bytes.len() - 2],
            "low strips the stored [0, 0] terminator"
        );
        assert_eq!(
            high.as_slice(),
            {
                let mut expected = low.clone();
                expected.push(0xFF);
                expected
            },
            "high extends the stripped low by one 0xFF sentinel"
        );
    }
}

#[cfg(test)]
mod chunk_budget_tests {
    use super::graph_batch_chunk_within_update_budget;
    use gleaph_instruction_budget::{
        MAX_UPDATE_CALL_INSTRUCTIONS, ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE,
        ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM,
    };

    /// The guard must consume the previously-dead `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM`
    /// (GAP-2026-08-04-001): the exact trip point is
    /// `used + CHUNK_ESTIMATE + HEADROOM >= 40B`, so these boundary pins fail if any term is
    /// dropped, reordered, or the comparison direction flips.
    #[test]
    fn chunk_budget_allows_a_fresh_message() {
        assert!(graph_batch_chunk_within_update_budget(0));
    }

    #[test]
    fn chunk_budget_trips_at_and_past_the_exact_boundary() {
        // `should_cutoff` compares with `>=`: the boundary value itself already has zero
        // slack, so the guard stops there and everything above it.
        let boundary = MAX_UPDATE_CALL_INSTRUCTIONS
            - ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE
            - ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM;
        assert!(boundary > 0);
        assert!(graph_batch_chunk_within_update_budget(boundary - 1));
        assert!(!graph_batch_chunk_within_update_budget(boundary));
        assert!(!graph_batch_chunk_within_update_budget(boundary + 1));
        // The headroom alone cannot start a chunk once it is the only thing left.
        assert!(!graph_batch_chunk_within_update_budget(
            MAX_UPDATE_CALL_INSTRUCTIONS
        ));
    }

    #[test]
    fn chunk_budget_trips_when_only_headroom_remains() {
        // Once `used` reaches 40B - 4B, the entire remaining budget IS the reserve: the
        // guard must stop so finalization, journal reads, and the response fit.
        let used = MAX_UPDATE_CALL_INSTRUCTIONS - ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM;
        assert!(!graph_batch_chunk_within_update_budget(used));
    }
}
