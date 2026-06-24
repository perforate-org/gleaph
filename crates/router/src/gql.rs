//! Router-side GQL parse, plan, index seed routing, and graph dispatch.

use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::{IcWirePlanQueryResult, decode_gql_params_blob};
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_gql_planner::{PhysicalPlan, PlanOp, build_block_plan_with_schema};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ClaimId, EffectId, ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::{
    IndexIntersectionRequest, IndexIntersectionResult, PostingHit, ValuePostingCount,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanResult, GqlExecutionMode, GqlQueryResult, GraphMutationJournalEntryWire, MutationId,
    MutationJournalState, MutationToken, MutationTokenShard, ReadMode, ShardEventSeq,
    UniqueClaimDispatch,
};
use ic_cdk::api::msg_caller;
use nohash_hasher::IntSet;

use crate::execution_path::check_adhoc_execution_path;
#[cfg(target_family = "wasm")]
use crate::facade::stable::label_stats::ClientMutationKey;
use crate::facade::stable::label_stats::RouterMutationShard;
use crate::facade::stable::reservation_catalog::ConfirmOutcome;
use crate::facade::store::RouterStore;
use crate::facade::store::uniqueness::{
    ConstrainedDispatchSplit, LocalUniqueClaim, plan_can_release,
};
use crate::federation::{
    AggregateIndexFastPath, FederatedMergeMode, SeedHits, ShardDispatch, ShardingPolicy,
    apply_federated_aggregate_having, collect_label_hits_for_shards,
    collect_label_intersection_hits_for_shards, empty_execute_plan_result,
    federated_dispatch_plan_blob, federated_merge_mode_from_plans,
    gql_query_result_from_label_live_count, gql_query_result_from_posting_counts,
    merge_execute_plan_result, packed_vertices_exceed_fast_path_budget,
    posting_hits_exceed_fast_path_budget, routings_to_dispatches, sharding_policy_for,
    split_label_and_property_anchors, try_aggregate_index_fast_path,
    try_label_count_telemetry_fast_path, vertex_label_live_count,
};
use crate::graph_client::{
    ack_label_stats_deltas_through, ack_unique_effects, execute_plan_on_graph,
    get_mutation_journal_entry, index_pending_min_mutation_id, list_pending_label_stats_deltas,
    read_unique_effect_proof, read_unique_release_effects,
};
use crate::index_catalog::graph_stats_for;
use crate::index_lookup::{IndexLookup, RouterIndexLookup};
use crate::planner_stats::RouterGraphStats;
use crate::rbac::{authorize_adhoc_gql, authorize_index_ddl};
use crate::seed::{IndexAnchor, SeedAnchorSet, SeedProbe};
use crate::state::RouterError;

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
    property_id: u32,
    payload_bytes: Vec<u8>,
    wire_label_ids: &[u16],
) -> Result<Vec<gleaph_graph_kernel::index::EdgePostingHit>, String> {
    if wire_label_ids.is_empty() {
        return index
            .lookup_edge_equal(property_id, payload_bytes, None)
            .await;
    }
    let mut merged = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for &wire in wire_label_ids {
        for hit in index
            .lookup_edge_equal(property_id, payload_bytes.clone(), Some(wire))
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
) -> Result<SeedHits, String> {
    match anchor {
        IndexAnchor::Equal(SeedProbe {
            property_id,
            payload_bytes,
            ..
        }) => Ok(SeedHits::Vertices(
            index
                .lookup_equal(*property_id, payload_bytes.clone())
                .await?,
        )),
        IndexAnchor::EdgeEqual(crate::seed::EdgeSeedProbe {
            property_id,
            payload_bytes,
            wire_label_ids,
            ..
        }) => Ok(SeedHits::Edges(
            lookup_edge_equal_wires(index, *property_id, payload_bytes.clone(), wire_label_ids)
                .await?,
        )),
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
    }
}

async fn resolve_seed_hits_from_anchors<I: IndexLookup + ?Sized>(
    index: &I,
    anchors: &[IndexAnchor],
    shard_ids: &[ShardId],
) -> Result<SeedHits, String> {
    if anchors.is_empty() {
        return Ok(SeedHits::Vertices(Vec::new()));
    }
    let first = lookup_anchor_hits(index, &anchors[0], shard_ids).await?;
    if first.is_empty() {
        return Ok(first);
    }
    if anchors.len() == 1 {
        return Ok(first);
    }
    match first {
        SeedHits::Vertices(mut accumulated) => {
            for anchor in &anchors[1..] {
                let SeedHits::Vertices(hits) = lookup_anchor_hits(index, anchor, shard_ids).await?
                else {
                    return Err("mixed vertex and edge anchors in seed prefix".into());
                };
                accumulated = intersect_posting_hits(vec![accumulated, hits]);
                if accumulated.is_empty() {
                    return Ok(SeedHits::Vertices(Vec::new()));
                }
            }
            Ok(SeedHits::Vertices(accumulated))
        }
        SeedHits::Edges(_) => Err("edge anchor cannot combine with additional anchors".into()),
    }
}

async fn lookup_hits_for_anchor<I: IndexLookup + ?Sized>(
    index: &I,
    anchor: &IndexAnchor,
) -> Result<SeedHits, String> {
    lookup_anchor_hits(index, anchor, &[]).await
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
                .count_postings_by_value(fast_path.property_id, fast_path.min_count, None)
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
                        .count_postings_by_value(fast_path.property_id, fast_path.min_count, filter)
                        .await?
                }
            }
        }
        (Some(vertex_label_id), []) => {
            index
                .count_postings_by_value_for_label(
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

pub async fn gql_query(query: String, params: Vec<u8>) -> Result<GqlQueryResult, RouterError> {
    run_gql(
        &query,
        &params,
        GqlExecutionMode::Query,
        "gql_query",
        false,
        None,
        ReadMode::Eventual,
    )
    .await
}

/// Read with an explicit ADR 0029 §5 consistency contract (Phase 3).
///
/// `ReadMode::Eventual` matches [`gql_query`]. `ReadMode::AtLeast(token)` enforces the
/// read-your-writes barrier: the read is served only once every shard in the token has
/// reached its label-stats and graph-index watermarks, otherwise a retryable
/// `RouterError::ProjectionLag` is returned without serving stale state.
/// `ReadMode::Canonical` is deferred and rejected.
pub async fn gql_query_with_consistency(
    query: String,
    params: Vec<u8>,
    read_mode: ReadMode,
) -> Result<GqlQueryResult, RouterError> {
    run_gql(
        &query,
        &params,
        GqlExecutionMode::Query,
        "gql_query_with_consistency",
        false,
        None,
        read_mode,
    )
    .await
}

pub async fn gql_execute(query: String, params: Vec<u8>) -> Result<u64, RouterError> {
    Ok(run_gql(
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_execute",
        false,
        None,
        ReadMode::Eventual,
    )
    .await?
    .row_count)
}

pub async fn gql_execute_idempotent(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<GqlQueryResult, RouterError> {
    let result = run_gql(
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_execute_idempotent",
        false,
        Some(&client_mutation_key),
        ReadMode::Eventual,
    )
    .await;
    // ADR 0029 Phase 4: a federated mutation that committed canonically but could not
    // finish projection inline (returned here as an error) is left non-terminal; arm the
    // recovery driver so it converges without the client retrying. Self-guarding no-op when
    // the saga already finalized.
    crate::recovery::arm_if_needed();
    result
}

/// Run a read-only program on the **update** path (higher cost; escape hatch only).
pub async fn force_gql_execute(query: String, params: Vec<u8>) -> Result<u64, RouterError> {
    Ok(run_gql(
        &query,
        &params,
        GqlExecutionMode::Update,
        "force_gql_execute",
        true,
        None,
        ReadMode::Eventual,
    )
    .await?
    .row_count)
}

/// ADR 0029 §5 (Phase 3) read barrier.
///
/// For `AtLeast(token)`, verify every shard named in the token has reached both its
/// label-stats projection cursor and its graph-index repair watermark before any read
/// shape is served. If any watermark is unmet, return a retryable
/// [`RouterError::ProjectionLag`] instead of serving a stale projection. `Eventual` is a
/// no-op; `Canonical` is deferred (Phase 3) and rejected so callers never silently get
/// `Eventual` semantics under a stronger label.
pub(crate) async fn enforce_read_consistency(
    store: &RouterStore,
    graph_id: GraphId,
    read_mode: &ReadMode,
) -> Result<(), RouterError> {
    let token = match read_mode {
        ReadMode::Eventual => return Ok(()),
        ReadMode::Canonical => {
            return Err(RouterError::InvalidArgument(
                "Canonical read mode is not yet implemented (ADR 0029 Phase 3 deferred); \
                 use Eventual or AtLeast(token)"
                    .into(),
            ));
        }
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

        // Graph-index watermark: the shard's repair journal must have drained past the
        // token's `mutation_id`. Index-satisfied iff `None` or `mutation_id < min_pending`.
        let entry = store.resolve_shard(graph_id, shard.shard_id)?;
        let min_pending = index_pending_min_mutation_id(entry.graph_canister)
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
) -> Result<GqlQueryResult, RouterError> {
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
        let stmt = ddl.map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
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
    authorize_adhoc_gql(&caller, flags)?;
    check_adhoc_execution_path(entrypoint, mode, flags, force)?;

    let store = RouterStore::new();
    let resolved = crate::graph_context::resolve_graph_context(&store, &program, caller)?;
    let seed = crate::graph_context::session_graph_seed(&store, resolved, caller);
    gleaph_gql::validate::validate_with_seed(&program, Some(&seed))
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    // ADR 0029 §5 (Phase 3): enforce the read consistency contract before serving any
    // read shape (label-count fast path, index seed, or graph-shard scan all flow through
    // here). The barrier is a no-op for `Eventual` and for the write path.
    enforce_read_consistency(&store, resolved.graph_id, &read_mode).await?;

    let tx = program
        .transaction_activity
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing transaction".into()))?;
    let block = tx
        .body
        .as_ref()
        .ok_or_else(|| RouterError::InvalidArgument("missing statement block".into()))?;

    if crate::facade::stable::graph_type_catalog::block_has_catalog_ddl(block) {
        crate::facade::stable::graph_type_catalog::apply_catalog_statement_block(block)?;
        if crate::facade::stable::graph_type_catalog::block_is_catalog_ddl_only(block) {
            return Ok(GqlQueryResult::row_count_only(0));
        }
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
    let plan = build_block_plan_with_schema(&dispatch.plan_block, Some(&stats), schema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
    let requires_write_path = plan.has_dml();
    if requires_write_path != flags.requires_write_path() {
        return Err(RouterError::InvalidArgument(
            "planner DML content does not match program classification".into(),
        ));
    }

    // ADR 0029 Phase 5: a federated bundle of more than one top-level DML statement is admitted only
    // when it provably has no cross-shard read; otherwise its partial-application semantics are
    // undefined. This pre-dispatch gate (the AST is the SSOT for "how many DML statements the user
    // wrote") is the single admission point and exempts the two structurally-safe shapes:
    //   * a completely-new INSERT-only bundle (contract 1) — placed on the graph's latest shard; and
    //   * a single-anchor threaded bundle (contract 1/2) — one leading index/label anchor, no other
    //     existing-state read. When its anchor resolves to one shard it runs there atomically
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

    let pmap =
        decode_gql_params_blob(params).map_err(|e| RouterError::InvalidArgument(e.to_string()))?;

    match v2 {
        crate::use_graph::UseGraphV2Dispatch::EffectiveGraph { plan } => {
            let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
                .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
            dispatch_plan_blob(
                dispatch.dispatch_graph_id,
                &plan_blob,
                std::slice::from_ref(&plan),
                &pmap,
                params,
                mode,
                client_mutation_key,
                &stats,
            )
            .await
        }
        crate::use_graph::UseGraphV2Dispatch::Single { graph_id, plan } => {
            let stats = graph_stats_for(graph_id);
            let plan_blob = encode_block_plans(std::slice::from_ref(&plan), requires_write_path)
                .map_err(|e| RouterError::InvalidArgument(e.to_string()))?;
            dispatch_plan_blob(
                graph_id,
                &plan_blob,
                std::slice::from_ref(&plan),
                &pmap,
                params,
                mode,
                client_mutation_key,
                &stats,
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
        rows_blob: Some(
            projected
                .encode_blob()
                .map_err(|e| RouterError::InvalidArgument(e.to_string()))?,
        ),
    })
}

fn decode_wire_result(result: GqlQueryResult) -> Result<IcWirePlanQueryResult, RouterError> {
    let blob = result.rows_blob.ok_or_else(|| {
        RouterError::InvalidArgument("multi-graph branch returned no rows_blob".into())
    })?;
    IcWirePlanQueryResult::decode_blob(&blob)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))
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
    let store = RouterStore::new();
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    if shards.is_empty() {
        return Err(RouterError::ShardNotRegistered);
    }
    let index =
        RouterIndexLookup::from_shards(graph_id, &shards).map_err(RouterError::InvalidArgument)?;
    dispatch_plan_blob_with_index(
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
        msg_caller(),
        stats,
    )
    .await
}

/// Attach the ADR 0029 federated mutation lifecycle phase to a DML result. The phase is
/// derived from the saga record, so it is only present when the caller tracks an idempotent
/// mutation via `client_mutation_key`; read queries and non-idempotent writes stay `None`.
fn attach_mutation_phase(
    result: GqlQueryResult,
    store: &RouterStore,
    caller: Principal,
    graph_id: GraphId,
    client_mutation_key: Option<&str>,
) -> GqlQueryResult {
    let Some(key) = client_mutation_key else {
        return result;
    };
    match store.router_mutation_record(caller, graph_id, key) {
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
) -> Result<GqlQueryResult, RouterError> {
    let has_dml = plans.iter().any(PhysicalPlan::has_dml);
    // ADR 0029 §6 (Phase 5 contract 1): a completely-new INSERT-only write has no index anchor and
    // no existing-state reads, so it is placed on the graph's latest shard rather than rejected.
    let pure_insert_write = has_dml && plans.iter().all(PhysicalPlan::is_pure_insert);
    if mode == GqlExecutionMode::Query && !has_dml {
        if let Some(label_path) = try_label_count_telemetry_fast_path(plans, stats, store, pmap) {
            let live_count = vertex_label_live_count(store, graph_id, label_path.vertex_label_id);
            return gql_query_result_from_label_live_count(&label_path, live_count)
                .map_err(RouterError::InvalidArgument);
        }
        if let Some(fast_path) = try_aggregate_index_fast_path(plans, stats, store, pmap)
            && let Some(counts) = execute_grouped_aggregate_fast_path(index, &fast_path)
                .await
                .map_err(RouterError::InvalidArgument)?
        {
            return gql_query_result_from_posting_counts(&fast_path, counts)
                .map_err(RouterError::InvalidArgument);
        }
    }
    let merge_mode = federated_merge_mode_from_plans(plans);
    let dispatch_plan_blob = federated_dispatch_plan_blob(shards.len(), plan_blob, plans, has_dml)
        .map_err(RouterError::InvalidArgument)?;
    let mutation_reservation = if has_dml {
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
    } else {
        None
    };
    let mutation_id = mutation_reservation.map(|reservation| reservation.mutation_id);

    if has_dml && let Some(key) = client_mutation_key {
        if let Some(row_count) = store.router_mutation_completed_row_count(caller, graph_id, key) {
            return Ok(attach_mutation_phase(
                GqlQueryResult::row_count_only(row_count),
                store,
                caller,
                graph_id,
                client_mutation_key,
            ));
        }
        reconcile_router_mutation_projection(store, caller, graph_id, key).await?;
        if let Some(row_count) = store.router_mutation_completed_row_count(caller, graph_id, key) {
            return Ok(attach_mutation_phase(
                GqlQueryResult::row_count_only(row_count),
                store,
                caller,
                graph_id,
                client_mutation_key,
            ));
        }
    }

    let saved_record =
        client_mutation_key.and_then(|key| store.router_mutation_record(caller, graph_id, key));
    let mut resolved_labels = match saved_record
        .as_ref()
        .and_then(|record| record.resolved_labels.clone())
    {
        Some(resolved_labels) => resolved_labels,
        None => match store.resolve_plan_labels(graph_id, plans) {
            Ok(resolved_labels) => resolved_labels,
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
    let mut resolved_properties = match saved_record
        .as_ref()
        .and_then(|record| record.resolved_properties.clone())
    {
        Some(resolved_properties) => resolved_properties,
        None => match store.resolve_plan_properties(graph_id, plans) {
            Ok(resolved_properties) => resolved_properties,
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

    // ADR 0030: refuse SET writes that would touch a constrained value before the two-phase
    // acquire/release protocol exists — they would otherwise reach the canonical write unguarded and
    // could create a duplicate once `CREATE CONSTRAINT` is published. Checked before dispatch so the
    // refusal records no envelope and reserves nothing.
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

    let mut dispatches: Vec<ShardDispatch> = if let Some(record) = saved_record.as_ref()
        && !record.shards.is_empty()
    {
        record
            .shards
            .iter()
            .map(|shard| ShardDispatch {
                shard_id: shard.shard_id,
                graph_canister: shard.graph_canister,
                seed_bindings_blob: shard.seed_bindings_blob.clone(),
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
        let routings = match seed_anchors {
            Some(set) => {
                let shard_ids: Vec<_> = shards.iter().map(|entry| entry.shard_id).collect();
                let hits = match resolve_seed_hits_from_anchors(index, &set.anchors, &shard_ids)
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
                    if let Some(key) = client_mutation_key {
                        store.record_router_mutation_completed_without_shards(
                            caller,
                            graph_id,
                            key,
                            resolved_labels.clone(),
                            resolved_properties.clone(),
                            0,
                        )?;
                    }
                    return Ok(attach_mutation_phase(
                        GqlQueryResult::row_count_only(0),
                        store,
                        caller,
                        graph_id,
                        client_mutation_key,
                    ));
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
    // single-anchor threaded bundles, neither of which performs a cross-shard read). A pure-insert is
    // placed on one shard; a single-anchor threaded bundle whose anchor fans out to many shards is
    // dispatched per shard below as a roll-forward saga — each shard atomic shard-locally, cross-shard
    // convergence roll-forward (no global rollback), resumed by idempotent retry / the recovery timer.
    if let (Some(key), Some(_)) = (client_mutation_key, mutation_id)
        && mutation_reservation.is_some_and(|reservation| reservation.routing_owner)
    {
        let envelope_shards = dispatches
            .iter()
            .map(|dispatch| {
                RouterMutationShard::new(
                    dispatch.shard_id,
                    dispatch.graph_canister,
                    dispatch.seed_bindings_blob.clone(),
                )
            })
            .collect();
        store.record_router_mutation_shards(
            caller,
            graph_id,
            key,
            resolved_labels.clone(),
            resolved_properties.clone(),
            envelope_shards,
        )?;
        if let Some(record) = store.router_mutation_record(caller, graph_id, key) {
            if let Some(saved_resolved_labels) = record.resolved_labels {
                resolved_labels = saved_resolved_labels;
            }
            if let Some(saved_resolved_properties) = record.resolved_properties {
                resolved_properties = saved_resolved_properties;
            }
            dispatches = record
                .shards
                .into_iter()
                .map(|shard| ShardDispatch {
                    shard_id: shard.shard_id,
                    graph_canister: shard.graph_canister,
                    seed_bindings_blob: shard.seed_bindings_blob,
                })
                .collect();
        }
    }
    let element_id_encoding_key = store.graph_element_id_encoding_key(graph_id)?.0;
    // ADR 0023 D1: the router (index definitions SSOT) supplies the indexed-property
    // catalog per operation so the shard never persists derived index state.
    let indexed_properties =
        crate::index_catalog::graph_stats_for(graph_id).to_indexed_property_catalog();
    // ADR 0031: the Router (vector-index definitions SSOT) supplies the indexed-embedding catalog
    // per operation, mirroring `indexed_properties`. The builder is fail-closed on the dynamic
    // per-graph gate: it exports specs only when dispatch is ready (global activation flag ON and
    // every live shard vector-attached), otherwise it is empty and derived vector sync stays inert.
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    let indexed_embeddings =
        crate::facade::stable::vector_index_catalog::to_indexed_embedding_catalog(
            graph_id,
            dispatch_ready,
        );

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

    let mut merged = empty_execute_plan_result();
    // ADR 0029 Phase 2: accumulate per-shard read-your-writes watermarks for the mutation
    // token as each shard completes (built live so it survives record compaction).
    let mut token_shards: Vec<MutationTokenShard> = Vec::new();
    for dispatch in dispatches {
        let result = match execute_plan_on_graph(
            dispatch.graph_canister,
            gleaph_graph_kernel::plan_exec::ExecutePlanArgs {
                target_shard_id: dispatch.shard_id,
                element_id_encoding_key,
                mutation_id,
                plan_blob: dispatch_plan_blob.clone(),
                params_blob: params.to_vec(),
                mode,
                seed_bindings_blob: dispatch.seed_bindings_blob.clone(),
                resolved_labels: Some(resolved_labels.clone()),
                resolved_properties: Some(resolved_properties.clone()),
                indexed_properties: Some(indexed_properties.clone()),
                unique_claims: dispatch_unique_claims.clone(),
                constrained_properties: dispatch_constrained_properties.clone(),
                local_unique_claims: dispatch_local_unique_claims.clone(),
                local_constrained_properties: dispatch_local_constrained_properties.clone(),
                indexed_embeddings: Some(indexed_embeddings.clone()),
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                if let Some(mutation_id) = mutation_id
                    && let Some(entry) = recover_mutation_outcome(
                        store,
                        graph_id,
                        dispatch.graph_canister,
                        dispatch.shard_id,
                        mutation_id,
                    )
                    .await?
                    && matches!(entry.state, MutationJournalState::Completed)
                {
                    if has_dml {
                        crate::bulk_ingest_finalize::maybe_finalize_hot_vertices_after_dml(
                            dispatch.graph_canister,
                            dispatch.shard_id,
                            plans,
                            &entry.hot_forward_vertices,
                        )
                        .await?;
                    }
                    merge_execute_plan_result(
                        &mut merged,
                        gleaph_graph_kernel::plan_exec::ExecutePlanResult {
                            row_count: entry.row_count,
                            rows_blob: None,
                            hot_forward_vertices: entry.hot_forward_vertices,
                        },
                        merge_mode.clone(),
                    )
                    .map_err(RouterError::InvalidArgument)?;
                    if let Some(key) = client_mutation_key {
                        store.record_router_mutation_shard_completed(
                            caller,
                            graph_id,
                            key,
                            dispatch.shard_id,
                            entry.row_count,
                        )?;
                        store.record_router_mutation_shard_projection_advanced(
                            caller,
                            graph_id,
                            key,
                            dispatch.shard_id,
                        )?;
                    }
                    token_shards.push(MutationTokenShard {
                        shard_id: dispatch.shard_id,
                        label_stats_seq: entry.emitted_delta_last_seq,
                    });
                    continue;
                }
                // ADR 0030 slice 10: a `ShardLocalGlobal` duplicate is detected on the owning shard
                // (no Router-side reservation Try), so its violation arrives here as a string. Re-type
                // it to the non-retryable `UniquenessViolation` the FederatedTcc path returns directly,
                // instead of a generic `InvalidArgument`.
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
            )
            .await?;
            token_shards.push(MutationTokenShard {
                shard_id: dispatch.shard_id,
                label_stats_seq: entry.emitted_delta_last_seq,
            });
        }
        if has_dml {
            crate::bulk_ingest_finalize::maybe_finalize_hot_vertices_after_dml(
                dispatch.graph_canister,
                dispatch.shard_id,
                plans,
                &result.hot_forward_vertices,
            )
            .await?;
        }
        if let Some(key) = client_mutation_key {
            store.record_router_mutation_shard_completed(
                caller,
                graph_id,
                key,
                dispatch.shard_id,
                result.row_count,
            )?;
            store.record_router_mutation_shard_projection_advanced(
                caller,
                graph_id,
                key,
                dispatch.shard_id,
            )?;
        }
        merge_execute_plan_result(&mut merged, result, merge_mode.clone())
            .map_err(RouterError::InvalidArgument)?;
    }
    // ADR 0030 slice 5a: Confirm the cross-shard uniqueness reservations now that every shard's
    // canonical write (and its pinned `Acquire`) is durable. Best-effort and idempotent — the
    // canonical write cannot be rolled back, so a read/ack failure leaves the reservation `Reserved`
    // for the slice-6 recovery reconciler rather than failing the (succeeded) mutation.
    if let Some(mutation_id) = mutation_id
        && !unique_claims.is_empty()
    {
        // ADR 0030 slice 7 (failure injection): trap in the post-dispatch callback before Confirm.
        // The shard's canonical write + pinned `Acquire` are already durable; only the Router-side
        // Confirm rolls back, leaving the reservation `Reserved` (commit-but-reply-lost) for recovery
        // to converge — Confirm is re-runnable and the reservation is never lost.
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
    // ADR 0030 slice 5b: reconcile the `Release` effects the constrained delete/remove pinned, now
    // that the canonical write is durable. Best-effort and idempotent — a removed reservation cannot
    // un-delete the element, and a held/failed release is left pinned for slice-6 recovery.
    if let Some(mutation_id) = mutation_id
        && !constrained_properties.is_empty()
    {
        reconcile_unique_releases(store, graph_id, mutation_id, &unique_release_targets).await;
    }
    if let FederatedMergeMode::Aggregate(spec) = &merge_mode {
        apply_federated_aggregate_having(&mut merged, spec, pmap)
            .map_err(RouterError::InvalidArgument)?;
    }
    // ADR 0029 Phase 2: issue a read-your-writes token for the idempotent mutation. The
    // index barrier is keyed by the monotonic mutation_id; label-stats barriers by each
    // shard's emitted delta seq. Enforcement is Phase 3.
    let token = mutation_id.map(|mutation_id| MutationToken {
        mutation_id,
        shards: token_shards,
    });
    if let Some(key) = client_mutation_key
        && let Some(row_count) = store.router_mutation_completed_row_count(caller, graph_id, key)
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
    Ok(attach_mutation_token(
        attach_mutation_phase(
            GqlQueryResult::from_merged(&merged),
            store,
            caller,
            graph_id,
            client_mutation_key,
        ),
        token,
    ))
}

/// Attach an ADR 0029 Phase 2 mutation token when one was issued for this dispatch.
fn attach_mutation_token(result: GqlQueryResult, token: Option<MutationToken>) -> GqlQueryResult {
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
        store.abandon_router_mutation_routing_reservation(caller, graph_id, key)?;
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

const LABEL_STATS_PROJECTION_BATCH_LIMIT: u32 = 1_000;

async fn advance_label_stats_projection_through(
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
    caller: Principal,
    graph_id: GraphId,
    client_key: &str,
) -> Result<(), RouterError> {
    let Some(record) = store.router_mutation_record(caller, graph_id, client_key) else {
        return Ok(());
    };
    for shard in record
        .shards
        .iter()
        .filter(|shard| shard.completed && !shard.projection_advanced)
    {
        let Some(entry) = recover_mutation_outcome(
            store,
            graph_id,
            shard.graph_canister,
            shard.shard_id,
            record.mutation_id,
        )
        .await?
        else {
            return Err(RouterError::InvalidArgument(format!(
                "mutation {} completed on router shard {} but graph journal is unavailable",
                record.mutation_id, shard.shard_id
            )));
        };
        if !matches!(entry.state, MutationJournalState::Completed) {
            return Err(RouterError::InvalidArgument(format!(
                "mutation {} completed on router shard {} but graph journal is not completed",
                record.mutation_id, shard.shard_id
            )));
        }
        store.record_router_mutation_shard_projection_advanced(
            caller,
            graph_id,
            client_key,
            shard.shard_id,
        )?;
    }
    Ok(())
}

async fn advance_mutation_label_stats_projection(
    store: &RouterStore,
    graph_id: GraphId,
    graph_canister: Principal,
    shard_id: ShardId,
    mutation_id: MutationId,
) -> Result<GraphMutationJournalEntryWire, RouterError> {
    let Some(entry) = get_mutation_journal_entry(graph_canister, mutation_id)
        .await
        .map_err(RouterError::InvalidArgument)?
    else {
        return Err(RouterError::InvalidArgument(format!(
            "graph shard {shard_id} did not persist mutation journal entry for mutation {mutation_id}"
        )));
    };
    if !matches!(entry.state, MutationJournalState::Completed) {
        return Err(RouterError::InvalidArgument(format!(
            "graph shard {shard_id} mutation {mutation_id} did not complete"
        )));
    }
    advance_label_stats_projection_through(
        store,
        graph_id,
        graph_canister,
        shard_id,
        entry.emitted_delta_last_seq,
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
) -> Result<Option<GraphMutationJournalEntryWire>, RouterError> {
    let Some(entry) = get_mutation_journal_entry(graph_canister, mutation_id)
        .await
        .map_err(RouterError::InvalidArgument)?
    else {
        return Ok(None);
    };
    if !matches!(entry.state, MutationJournalState::Completed) {
        return Ok(None);
    }
    advance_label_stats_projection_through(
        store,
        graph_id,
        graph_canister,
        shard_id,
        entry.emitted_delta_last_seq,
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
/// completed+projected; once every shard is projected the record finalizes (terminal). If a
/// shard's canonical write has not committed (`CanonicalPending`), a diagnostic is recorded
/// and the shard is left for explicit, retry-driven recovery — re-dispatching canonical DML
/// from a background driver is out of scope precisely because it is the one operation that
/// risks double-apply.
///
/// Idempotent and bounded: safe to call concurrently with a client retry (both paths use
/// cursor-guarded projection advancement and idempotent record mutators).
#[cfg(target_family = "wasm")]
pub(crate) async fn recover_mutation_record(
    store: &RouterStore,
    key: &ClientMutationKey,
) -> Result<(), RouterError> {
    let Some(record) = store.router_mutation_record(key.caller, key.graph_id, &key.client_key)
    else {
        return Ok(());
    };
    if record.is_terminal() {
        return Ok(());
    }
    let mutation_id = record.mutation_id;
    for shard in &record.shards {
        if shard.completed && shard.projection_advanced {
            continue;
        }
        match recover_mutation_outcome(
            store,
            key.graph_id,
            shard.graph_canister,
            shard.shard_id,
            mutation_id,
        )
        .await?
        {
            Some(entry) => {
                if !shard.completed {
                    store.record_router_mutation_shard_completed(
                        key.caller,
                        key.graph_id,
                        &key.client_key,
                        shard.shard_id,
                        entry.row_count,
                    )?;
                }
                store.record_router_mutation_shard_projection_advanced(
                    key.caller,
                    key.graph_id,
                    &key.client_key,
                    shard.shard_id,
                )?;
            }
            None => {
                store.record_router_mutation_last_error(
                    key,
                    format!(
                        "shard {} canonical write not yet committed; retry the idempotent \
                         mutation to resume",
                        shard.shard_id
                    ),
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::rc::Rc;

    use crate::federation::SeedHits;
    use crate::index_lookup::IndexLookup;
    use candid::{Decode, Principal};
    use gleaph_gql::Value;
    use gleaph_gql::ast::CmpOp;
    use gleaph_gql_ic::{IcWirePlanQueryResult, IcWireValue};
    use gleaph_gql_planner::plan::ScanValue;
    use gleaph_gql_planner::wire::encode_block_plans;
    use gleaph_gql_planner::{NodeLabelRef, PhysicalPlan, PlanOp};
    use gleaph_graph_kernel::index::{
        EdgePostingHit, IndexLabelIntersectionRequest, LabelLookupPageResult, PostingHit,
        ValuePostingCount,
    };
    use gleaph_graph_kernel::plan_exec::{
        ExecutePlanResult, GqlExecutionMode, GqlQueryResult, LabelStatsDelta,
        LabelStatsDeltaEventWire, MutationToken, MutationTokenShard, ReadMode, SeedBindingsWire,
    };

    use crate::facade::stable::graph_catalog::lookup_graph_id;
    use crate::facade::store::RouterStore;
    use crate::federation::{
        collect_label_intersection_hits_for_shards, resolve_seed_routings_multi,
        routings_to_dispatches,
    };
    use crate::gql::{
        dispatch_plan_blob_with_index, enforce_read_consistency, request_fingerprint,
    };
    use crate::init::RouterInitArgs;
    use crate::planner_stats::RouterGraphStats;
    use crate::seed::SeedAnchorSet;
    use crate::seed::{IndexAnchor, SeedProbe, seeds_for_local_shard};
    use crate::state::RouterError;
    use crate::types::{
        AdminRegisterShardArgs, GraphRegistryEntry, GraphStatus, ProvisioningState,
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
            vector_index_canister: None,
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
                    graph_name: name.to_owned(),
                    canister_id: Principal::management_canister(),
                    owner: admin,
                    admins: BTreeSet::new(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
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

    fn store_with_shards() -> RouterStore {
        let store = RouterStore::new();
        store.init_from_args(&RouterInitArgs {
            issuing_principal: Principal::anonymous(),
            initial_admins: vec![],
        });
        let admin = Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[admin]);
        register_test_graph(&store, admin, "tenant.main");
        for (shard_id, graph_byte) in [(ShardId::new(0), 1u8), (ShardId::new(1), 4)] {
            futures::executor::block_on(store.admin_register_shard(
                admin,
                AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: graph_principal(graph_byte),
                    index_canister: graph_principal(2),
                    logical_graph_name: "tenant.main".into(),
                },
            ))
            .expect("register shard");
        }
        store
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
            _property_id: u32,
            _value: Vec<u8>,
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
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
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
            _property_id: u32,
            _value: Vec<u8>,
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
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
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
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PostingHit>, String>> + '_>> {
            self.equal_calls.set(self.equal_calls.get() + 1);
            let hits = self.equal_hits.clone();
            Box::pin(async move { Ok(hits) })
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
            _property_id: u32,
            _value: Vec<u8>,
            _label_id: Option<u16>,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<EdgePostingHit>, String>> + '_>> {
            Box::pin(async move { Ok(Vec::new()) })
        }

        fn count_postings_by_value(
            &self,
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
        store
            .admin_intern_property(admin, "tenant.main", "region")
            .expect("intern region");
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
        store
            .admin_intern_property(admin, "tenant.main", "uid")
            .expect("intern uid");
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
            .router_mutation_record(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
            )
            .expect("mutation record");
        assert_eq!(record.mutation_id, 1);
        assert_eq!(
            record.request_fingerprint,
            request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update)
        );
        assert!(!record.routing_in_progress);
        assert!(record.shards.is_empty());
        assert!(record.completed_row_count.is_none());

        let retry = store
            .reserve_mutation_id_for_client_key(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
                request_fingerprint(&plan_blob, &[], GqlExecutionMode::Update),
            )
            .expect("retry reservation");
        assert_eq!(retry.mutation_id, record.mutation_id);
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
        let rows_blob = IcWirePlanQueryResult {
            rows: vec![gleaph_gql_ic::IcWirePlanQueryRow {
                columns: vec![("n".into(), IcWireValue::Int64(7))],
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
            .router_mutation_record(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
            )
            .expect("mutation record");
        assert_eq!(record.completed_row_count, Some(0));
        assert!(!record.routing_in_progress);
        assert!(record.shards.is_empty());

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
            .router_mutation_record(
                Principal::anonymous(),
                tenant_main_graph_id(),
                "client-key-1",
            )
            .expect("mutation record");
        assert_eq!(record.mutation_id, 1);
        assert!(!record.routing_in_progress);
        assert!(record.completed_row_count.is_none());
        assert_eq!(record.shards.len(), 1);
        assert_eq!(record.shards[0].shard_id, ShardId::new(0));
        assert_eq!(record.shards[0].graph_canister, graph_principal(1));
        assert!(!record.shards[0].completed);

        let resolved = record.resolved_labels.expect("resolved labels");
        assert_eq!(resolved.vertex.len(), 1);
        assert_eq!(resolved.vertex[0].name, "Person");
        assert_eq!(resolved.vertex[0].id.raw(), 1);

        let seed_blob = record.shards[0]
            .seed_bindings_blob
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
    fn resolve_seed_routings_multi_rejects_unknown_shard() {
        let store = store_with_shards();
        let probe = SeedProbe {
            variable: "u".into(),
            property: "uid".into(),
            property_id: 1,
            payload_bytes: vec![],
        };
        let hits = vec![PostingHit {
            shard_id: ShardId::new(99),
            vertex_id: 1,
        }];
        let err = resolve_seed_routings_multi(
            &store,
            SeedHits::Vertices(hits),
            tenant_main_graph_id(),
            IndexAnchor::Equal(probe),
        )
        .expect_err("unknown shard");
        assert!(matches!(err, RouterError::ShardNotRegistered));
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
        assert_eq!(set.anchors.len(), 2);

        let fake = compound_seed_fake_index();
        let hits = futures::executor::block_on(super::resolve_seed_hits_from_anchors(
            &fake,
            &set.anchors,
            &[ShardId::new(0), ShardId::new(1)],
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

    // ADR 0029 §5 (Phase 3) read barrier decision logic. The index branch performs an
    // inter-canister query (covered by PocketIC); these host tests pin the local decisions:
    // `Eventual` no-op, `Canonical` rejection, and the label-stats lag short-circuit.
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
    fn read_barrier_canonical_is_rejected() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        let err = futures::executor::block_on(enforce_read_consistency(
            &store,
            graph_id,
            &ReadMode::Canonical,
        ))
        .expect_err("canonical is deferred");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }

    #[test]
    fn read_barrier_atleast_label_stats_lag_returns_retryable_projection_lag() {
        let store = store_with_shards();
        let graph_id = tenant_main_graph_id();
        // Cursor defaults to 0; require seq 5 on shard 0 → unmet, so the barrier must
        // return ProjectionLag *before* any inter-canister index call.
        let token = MutationToken {
            mutation_id: 1,
            shards: vec![MutationTokenShard {
                shard_id: ShardId::new(0),
                label_stats_seq: Some(5),
            }],
        };
        let err = futures::executor::block_on(enforce_read_consistency(
            &store,
            graph_id,
            &ReadMode::AtLeast(token),
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

    /// ADR 0030 slice 5a: the single-shard admission gate must run *before* the saga envelope record
    /// and Try, so a constrained insert that resolves to more than one shard is refused with no
    /// reservation residue. The only way to reach the gate with >1 dispatch is a pre-recorded
    /// multi-shard saga envelope (fresh routing of a pure insert always lands on one shard), so the
    /// test seeds that envelope and drives the real dispatch path.
    #[test]
    fn single_shard_gate_rejects_constrained_multishard_dispatch_without_reservation_residue() {
        use crate::facade::stable::constraint_name_catalog::lookup_constraint_name_id;
        use crate::facade::stable::label_stats::RouterMutationShard;
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
                caller,
                graph_id,
                key,
                resolved_labels,
                resolved_properties,
                vec![
                    RouterMutationShard::new(ShardId::new(0), graph_principal(1), None),
                    RouterMutationShard::new(ShardId::new(1), graph_principal(4), None),
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
}
