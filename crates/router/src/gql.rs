//! Router-side GQL parse, plan, index seed routing, and graph dispatch.

use std::collections::{BTreeMap, HashSet};

use candid::Principal;
use gleaph_gql::parser;
use gleaph_gql::program_modification::classify_program;
use gleaph_gql::type_check::NoSchema;
use gleaph_gql_ic::{IcWirePlanQueryResult, decode_gql_params_blob};
use gleaph_gql_planner::wire::encode_block_plans;
use gleaph_gql_planner::{PhysicalPlan, PlanOp, build_block_plan_with_schema};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardId, ShardRegistryEntry};
use gleaph_graph_kernel::index::{
    IndexIntersectionRequest, IndexIntersectionResult, PostingHit, ValuePostingCount,
};
use gleaph_graph_kernel::plan_exec::{
    ExecutePlanResult, GqlExecutionMode, GqlQueryResult, GraphMutationJournalEntryWire, MutationId,
    MutationJournalState, ShardEventSeq,
};
use ic_cdk::api::msg_caller;

use crate::execution_path::check_adhoc_execution_path;
use crate::facade::stable::label_stats::RouterMutationShard;
use crate::facade::store::RouterStore;
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
    ack_label_stats_deltas_through, execute_plan_on_graph, get_mutation_journal_entry,
    list_pending_label_stats_deltas,
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
    let mut sets: Vec<HashSet<u64>> = hit_sets
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

    let mut sets: Vec<std::collections::HashSet<u64>> = Vec::with_capacity(anchors.len());
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
    )
    .await?
    .row_count)
}

pub async fn gql_execute_idempotent(
    query: String,
    params: Vec<u8>,
    client_mutation_key: String,
) -> Result<u64, RouterError> {
    Ok(run_gql(
        &query,
        &params,
        GqlExecutionMode::Update,
        "gql_execute_idempotent",
        false,
        Some(&client_mutation_key),
    )
    .await?
    .row_count)
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
    )
    .await?
    .row_count)
}

async fn run_gql(
    query: &str,
    params: &[u8],
    mode: GqlExecutionMode,
    entrypoint: &str,
    force: bool,
    client_mutation_key: Option<&str>,
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
            return Ok(GqlQueryResult::row_count_only(row_count));
        }
        reconcile_router_mutation_projection(store, caller, graph_id, key).await?;
        if let Some(row_count) = store.router_mutation_completed_row_count(caller, graph_id, key) {
            return Ok(GqlQueryResult::row_count_only(row_count));
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
                    return Ok(GqlQueryResult::row_count_only(0));
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

    let mut merged = empty_execute_plan_result();
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
                    continue;
                }
                return Err(RouterError::InvalidArgument(err));
            }
        };
        if let Some(mutation_id) = mutation_id {
            advance_mutation_label_stats_projection(
                store,
                graph_id,
                dispatch.graph_canister,
                dispatch.shard_id,
                mutation_id,
            )
            .await?;
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
    if let FederatedMergeMode::Aggregate(spec) = &merge_mode {
        apply_federated_aggregate_having(&mut merged, spec, pmap)
            .map_err(RouterError::InvalidArgument)?;
    }
    if let Some(key) = client_mutation_key
        && let Some(row_count) = store.router_mutation_completed_row_count(caller, graph_id, key)
    {
        return Ok(GqlQueryResult::row_count_only(row_count));
    }
    Ok(GqlQueryResult::from_merged(&merged))
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
        LabelStatsDeltaEventWire, SeedBindingsWire,
    };

    use crate::facade::stable::graph_catalog::lookup_graph_id;
    use crate::facade::store::RouterStore;
    use crate::federation::{
        collect_label_intersection_hits_for_shards, resolve_seed_routings_multi,
        routings_to_dispatches,
    };
    use crate::gql::{dispatch_plan_blob_with_index, request_fingerprint};
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
}
