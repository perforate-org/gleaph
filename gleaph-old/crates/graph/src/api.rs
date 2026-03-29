use candid::encode_one;
use gleaph_algo::{
    AlgoOutcome,
    bfs::{self, BfsConfig},
    budget::IcBudget,
    pagerank::{self, PageRankConfig},
    recommend::{self, RecommendConfig},
    sssp::{self, SsspConfig},
};
use gleaph_pma::{AbpPropertyStore, AbpSecondaryEqIndex};
#[allow(unused_imports)]
use gleaph_types::{
    AccessLevel, AclEntry, AlgorithmKind, BfsResult, BfsResultWithContinuation, CanisterInfo,
    CertifiedResponse, ContinuationResult, ContinuationToken, EdgeData, EdgeInfo, EntityType,
    ExecuteGqlResult, GleaphError, GraphAlias, GraphFingerprint, GraphInfo, GraphStats, IndexType,
    MutationResult, MutationResultWithContinuation, OperationalMetrics, PageRankResult,
    PageRankResultWithContinuation, PlannerStats, QueryResult, QueryResultWithContinuation,
    Recommendation, SsspResult, SsspResultWithContinuation, UsageQuota, VertexData, VertexIdSet,
};

use crate::state::{with_state, with_state_mut};

/// Returns all neighbors for a vertex as API-friendly edge info values.
pub fn get_neighbors(vertex_id: u32) -> Vec<EdgeInfo> {
    with_state(|g| {
        g.collect_neighbors_filtered(vertex_id)
            .unwrap_or_default()
            .into_iter()
            .map(|e| EdgeInfo {
                target: e.target,
                weight: e.weight,
                timestamp: e.timestamp,
            })
            .collect()
    })
}

/// Returns current graph statistics.
pub fn get_stats() -> GraphStats {
    with_state(|g| g.stats())
}

pub fn get_stats_certified() -> CertifiedResponse<GraphStats> {
    crate::certification::get_stats_certified()
}

/// Returns cached planner statistics (label cardinality, property selectivity, etc.).
/// Call `compute_graph_stats()` first to populate the selectivity estimates.
pub fn get_planner_stats() -> PlannerStats {
    with_state(|g| g.planner_stats())
}

/// Returns diagnostic information about the canister's current state.
///
/// Useful for monitoring, canary deployments, and verifying upgrade success.
pub fn get_canister_info() -> CanisterInfo {
    let layout_version = with_state(|g| gleaph_pma::layout::read_header(&g.mem).version);
    CanisterInfo {
        layout_version,
        wasm_hash: String::new(),
        uptime_ns: {
            #[cfg(target_arch = "wasm32")]
            {
                ic_cdk::api::time()
            }
            #[cfg(not(target_arch = "wasm32"))]
            0
        },
        last_upgrade_ns: 0, // Updated by post_upgrade hook in lib.rs
    }
}

/// Returns operational metrics (query/mutation/algorithm counters, memory usage).
pub fn get_metrics() -> OperationalMetrics {
    OperationalMetrics {
        stable_memory_bytes: {
            #[cfg(target_arch = "wasm32")]
            {
                ic_cdk::stable::stable_size() * 65536
            }
            #[cfg(not(target_arch = "wasm32"))]
            {
                0
            }
        },
        ..crate::state::with_metrics(|m| m.clone())
    }
}

/// Returns the current per-tenant usage quotas.
pub fn get_quota() -> UsageQuota {
    crate::state::get_quota()
}

/// Configures per-tenant usage quotas. Only callable by the controller (enforced by IC).
pub fn set_quota(quota: UsageQuota) {
    crate::state::set_quota(quota);
}

/// Recomputes sample-based property-selectivity estimates and persists them in the overlay
/// snapshot.  This is an update endpoint because it mutates the cached selectivity field.
pub fn compute_graph_stats() -> PlannerStats {
    with_state_mut(|g| {
        g.compute_property_selectivity();
        g.planner_stats()
    })
}

fn sync_stable_abp_snapshots_if_needed() -> Result<(), GleaphError> {
    let (has_props, has_vertex_eq_index) = with_state(|g| {
        let has_idx = g.list_property_indexes().into_iter().any(|idx| {
            matches!(idx.entity_type, EntityType::Vertex)
                && matches!(idx.index_type, IndexType::Equality)
        });
        (g.has_overlay_properties(), has_idx)
    });

    let (prop_off, prop_len, _sec_off, _sec_len, _base) = crate::state::current_regions_meta()?;
    if has_props && (prop_off == 0 || prop_len == 0) {
        crate::state::ensure_property_store_reserved_region_initialized(0)?;
    }
    if has_props {
        crate::state::rebuild_property_store_abp_snapshot()?;
    }

    if !has_vertex_eq_index {
        return Ok(());
    }
    let (_prop_off2, _prop_len2, sec_off2, sec_len2, _base2) =
        crate::state::current_regions_meta()?;
    if sec_off2 == 0 || sec_len2 == 0 {
        return Ok(());
    }
    crate::state::rebuild_secondary_index_abp_snapshot()
}

fn sync_stable_abp_vertex_visibility_incremental_if_needed(
    vertex_ids: &[u32],
) -> Result<(), GleaphError> {
    if vertex_ids.is_empty() {
        return Ok(());
    }
    let mut ids = vertex_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();

    let (has_props, has_vertex_eq_index) = with_state(|g| {
        let has_idx = g.list_property_indexes().into_iter().any(|idx| {
            matches!(idx.entity_type, EntityType::Vertex)
                && matches!(idx.index_type, IndexType::Equality)
        });
        (g.has_overlay_properties(), has_idx)
    });

    let (mut prop_off, mut prop_len, mut sec_off, mut sec_len, _base) =
        crate::state::current_regions_meta()?;
    if has_props && (prop_off == 0 || prop_len == 0) {
        crate::state::ensure_property_store_reserved_region_initialized(0)?;
        (prop_off, prop_len, sec_off, sec_len, _) = crate::state::current_regions_meta()?;
    }

    if (!has_props || prop_off == 0 || prop_len == 0)
        && (!has_vertex_eq_index || sec_off == 0 || sec_len == 0)
    {
        return Ok(());
    }

    with_state_mut(|g| {
        let mut mem = g.mem.clone();
        if has_props && prop_off > 0 && prop_len > 0 {
            let mut store = AbpPropertyStore::from_memory(mem, prop_off).map_err(|e| {
                GleaphError::ExecutionError(format!("open stable property-store ABP: {e}"))
            })?;
            for &vid in &ids {
                if g.is_vertex_tombstoned(vid) {
                    if let Some(props) = g.get_vertex_props(vid) {
                        g.apply_vertex_props_to_abp_property_store(&mut store, vid, &props, false)?;
                    }
                    continue;
                }
                if let Some(props) = g.get_vertex_props(vid) {
                    g.apply_vertex_props_to_abp_property_store(&mut store, vid, &props, true)?;
                }
            }
            mem = store.into_memory();
        }
        if has_vertex_eq_index && sec_off > 0 && sec_len > 0 {
            let mut idx = AbpSecondaryEqIndex::from_memory(mem, sec_off).map_err(|e| {
                GleaphError::ExecutionError(format!("open stable secondary-index ABP: {e}"))
            })?;
            for &vid in &ids {
                if g.is_vertex_tombstoned(vid) {
                    if let Some(props) = g.get_vertex_props(vid) {
                        g.apply_vertex_props_to_abp_secondary_eq_index(
                            &mut idx, vid, &props, false,
                        )?;
                    }
                    continue;
                }
                if let Some(props) = g.get_vertex_props(vid) {
                    g.apply_vertex_props_to_abp_secondary_eq_index(&mut idx, vid, &props, true)?;
                }
            }
            mem = idx.into_memory();
        }
        g.mem = mem;
        Ok(())
    })?;

    crate::state::refresh_reserved_abp_region_lengths()
}

fn sync_stable_abp_legacy_edge_incremental_if_needed(
    vertex_ids: &[u32],
    edges: &[(u32, u32)],
) -> Result<(), GleaphError> {
    // Secondary vertex-equality indexes only depend on vertex properties, so reuse the vertex
    // visibility incremental path for endpoint revivals first.
    sync_stable_abp_vertex_visibility_incremental_if_needed(vertex_ids)?;

    if edges.is_empty() {
        return Ok(());
    }

    let mut pairs = edges.to_vec();
    pairs.sort_unstable();
    pairs.dedup();

    let has_props = with_state(|g| g.has_overlay_properties());
    if !has_props {
        return Ok(());
    }
    let (prop_off, prop_len, _sec_off, _sec_len, _base) = crate::state::current_regions_meta()?;
    if prop_off == 0 || prop_len == 0 {
        return Ok(());
    }

    with_state_mut(|g| {
        let mut store = AbpPropertyStore::from_memory(g.mem.clone(), prop_off).map_err(|e| {
            GleaphError::ExecutionError(format!("open stable property-store ABP: {e}"))
        })?;
        for (src, dst) in pairs {
            // Legacy add_edge uses unlabeled input, but a preexisting tombstoned edge may retain a label.
            let label = g.edge_label(src, dst).unwrap_or_default();
            if let Some(rec) = g.edge_record(src, dst, Some(&label))
                && !rec.props.is_empty()
            {
                g.apply_edge_props_to_abp_property_store(
                    &mut store, src, dst, &label, &rec.props, true,
                )?;
            }
        }
        g.mem = store.into_memory();
        Ok(())
    })?;

    crate::state::refresh_reserved_abp_region_lengths()
}

/// Ensures a vertex exists and returns the total vertex count.
pub fn add_vertex(vertex: VertexData) -> Result<u64, GleaphError> {
    let changed = with_state_mut(|g| g.revive_vertex_changed(vertex.id))?;
    if changed {
        sync_stable_abp_vertex_visibility_incremental_if_needed(&[vertex.id])
            .or_else(|_| sync_stable_abp_snapshots_if_needed())?;
        crate::certification::certify_stats(get_stats());
        crate::certification::invalidate_algo_caches();
    }
    Ok(with_state(|g| g.vertex_count()))
}

/// Inserts one edge and returns the total edge count.
pub fn add_edge(edge: EdgeData) -> Result<u64, GleaphError> {
    let mut mutated = false;
    let mut changed_vertices: Vec<u32> = Vec::new();
    let result = with_state_mut(|g| {
        // Preserve legacy behavior across GQL logical deletes: inserting an edge should make the
        // endpoints visible again if they were only tombstoned.
        let src_changed = g.revive_vertex_changed(edge.src)?;
        let dst_changed = g.revive_vertex_changed(edge.dst)?;
        mutated |= src_changed;
        mutated |= dst_changed;
        if src_changed {
            changed_vertices.push(edge.src);
        }
        if dst_changed {
            changed_vertices.push(edge.dst);
        }
        if g.revive_edge_by_endpoints(edge.src, edge.dst)? {
            g.update_edge_payload_by_endpoints(edge.src, edge.dst, edge.weight, edge.timestamp)?;
            return Ok(());
        }
        // Prevent ambiguous parallel-edge states with the GQL overlay: legacy inserts against an
        // existing endpoint pair refresh the payload instead of creating a duplicate PMA entry.
        if g.update_edge_payload_by_endpoints(edge.src, edge.dst, edge.weight, edge.timestamp)? {
            return Ok(());
        }
        g.create_edge(
            edge.src,
            edge.dst,
            None,
            Vec::new(),
            edge.weight,
            edge.timestamp,
        )
    });
    if mutated || result.is_ok() {
        sync_stable_abp_legacy_edge_incremental_if_needed(
            &changed_vertices,
            &[(edge.src, edge.dst)],
        )
        .or_else(|_| sync_stable_abp_snapshots_if_needed())?;
        crate::certification::certify_stats(get_stats());
        crate::certification::invalidate_algo_caches();
    }
    result?;
    Ok(with_state(|g| g.edge_count()))
}

/// Inserts multiple vertices sequentially and returns the total vertex count.
pub fn bulk_insert_vertices(vertices: Vec<VertexData>) -> Result<u64, GleaphError> {
    let mut any_committed = false;
    let mut changed_ids: Vec<u32> = Vec::new();
    let mut err: Option<GleaphError> = None;
    for vertex in vertices {
        match with_state_mut(|g| g.revive_vertex_changed(vertex.id)) {
            Ok(changed) => {
                any_committed |= changed;
                if changed {
                    changed_ids.push(vertex.id);
                }
            }
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    if any_committed {
        sync_stable_abp_vertex_visibility_incremental_if_needed(&changed_ids)
            .or_else(|_| sync_stable_abp_snapshots_if_needed())?;
        crate::certification::certify_stats(get_stats());
        crate::certification::invalidate_algo_caches();
    }
    match err {
        Some(e) => Err(e),
        None => Ok(with_state(|g| g.vertex_count())),
    }
}

/// Inserts multiple edges sequentially and returns the total edge count.
pub fn bulk_insert_edges(edges: Vec<EdgeData>) -> Result<u64, GleaphError> {
    if edges.is_empty() {
        return Ok(with_state(|g| g.edge_count()));
    }

    let mut changed_vertices: Vec<u32> = Vec::new();
    let mut touched_edges: Vec<(u32, u32)> = Vec::new();
    let mut new_edges: Vec<(usize, u32, u32, f32, u64)> = Vec::new();

    // Phase 1: Batch vertex revival & classify edges (revive/update/new).
    // Expand vertices once for the max vertex ID in the batch.
    let max_vid = edges.iter().map(|e| e.src.max(e.dst)).max().unwrap_or(0);
    with_state_mut(|g| g.ensure_vertex(max_vid))?;

    // Revive tombstoned vertices in batch.
    let mut vertex_ids = VertexIdSet::new();
    for edge in &edges {
        vertex_ids.insert(edge.src);
        vertex_ids.insert(edge.dst);
    }
    with_state_mut(|g| -> Result<(), GleaphError> {
        for vid in &vertex_ids {
            if g.revive_vertex_changed(vid)? {
                changed_vertices.push(vid);
            }
        }
        Ok(())
    })?;

    // Build existing-edge set for dedup.
    let (existing_edges, high_degree_vertices) = with_state(|g| {
        let src_set: std::collections::HashSet<u32> = edges.iter().map(|e| e.src).collect();
        g.build_existing_edge_set(&src_set)
    })?;

    // Classify each edge: revive existing tombstoned, update existing payload, or new.
    let mut batch_seen = std::collections::HashSet::new();
    let mut err: Option<GleaphError> = None;
    for (i, edge) in edges.iter().enumerate() {
        let pair = (edge.src, edge.dst);
        if !batch_seen.insert(pair) {
            // Batch-internal duplicate → skip.
            continue;
        }

        let exists = if high_degree_vertices.contains(&edge.src) {
            with_state(|g| {
                g.collect_neighbors(edge.src)
                    .map(|ns| ns.iter().any(|e| e.target == edge.dst))
                    .unwrap_or(false)
            })
        } else {
            existing_edges.contains(&pair)
        };

        if exists {
            // Try to revive tombstoned edge or update payload (legacy semantics: preserve labels/props).
            let result = with_state_mut(|g| {
                if g.revive_edge_by_endpoints(edge.src, edge.dst)? {
                    g.update_edge_payload_by_endpoints(
                        edge.src,
                        edge.dst,
                        edge.weight,
                        edge.timestamp,
                    )?;
                    return Ok(());
                }
                if g.update_edge_payload_by_endpoints(
                    edge.src,
                    edge.dst,
                    edge.weight,
                    edge.timestamp,
                )? {
                    return Ok(());
                }
                Ok(())
            });
            touched_edges.push(pair);
            if let Err(e) = result {
                err = Some(e);
                break;
            }
        } else {
            // Genuinely new edge → queue for bulk_insert_raw (label_id=0 for unlabeled).
            new_edges.push((i, edge.src, edge.dst, edge.weight, edge.timestamp));
            touched_edges.push(pair);
        }
    }

    if let Some(e) = err {
        return Err(e);
    }

    // Phase 2: Bulk-insert all new edges via bulk_insert_raw.
    if !new_edges.is_empty() {
        let raw: Vec<(u32, u32, u32, f32, u64)> = new_edges
            .iter()
            .map(|&(_, s, d, w, t)| (s, d, 0u32, w, t))
            .collect();
        with_state_mut(|g| g.bulk_insert_raw(&raw))?;
    }

    // Phase 3: Sync ABP snapshots and certify (once).
    if !touched_edges.is_empty() || !changed_vertices.is_empty() {
        sync_stable_abp_legacy_edge_incremental_if_needed(&changed_vertices, &touched_edges)
            .or_else(|_| sync_stable_abp_snapshots_if_needed())?;
        crate::certification::certify_stats(get_stats());
        crate::certification::invalidate_algo_caches();
    }

    Ok(with_state(|g| g.edge_count()))
}

pub fn query_gql(
    gql: String,
    params: Option<gleaph_types::PropertyMap>,
) -> Result<QueryResultWithContinuation, GleaphError> {
    let full_result = if let Some(pm) = params {
        let params_map: std::collections::HashMap<String, gleaph_types::Value> =
            pm.into_iter().collect();
        match crate::gql_bridge::query_paged_with_params(&gql, &params_map) {
            Ok(r) => r,
            Err(e) => {
                crate::state::increment_rejected_count();
                return Err(e);
            }
        }
    } else {
        match crate::gql_bridge::query_paged(&gql) {
            Ok(r) => r,
            Err(e) => {
                crate::state::increment_rejected_count();
                return Err(e);
            }
        }
    };
    crate::state::increment_query_count();

    // Auto-fallback: if result fits in one page, return directly; otherwise page with cursor.
    if full_result.rows.len() <= CURSOR_PAGE_SIZE {
        return Ok(QueryResultWithContinuation {
            result: full_result,
            continuation: None,
        });
    }

    let fp = current_graph_fingerprint();
    let first_page = QueryResult {
        columns: full_result.columns.clone(),
        rows: full_result.rows[..CURSOR_PAGE_SIZE].to_vec(),
        stats: full_result.stats.clone(),
        warnings: full_result.warnings.clone(),
    };
    let cursor = GqlQueryCursor {
        query: Some(gql),
        prepared_name: None,
        prepared_params: None,
        prepared_sort: None,
        offset: CURSOR_PAGE_SIZE,
        page_size: CURSOR_PAGE_SIZE,
        total_rows: full_result.rows.len(),
    };
    let token = encode_checkpoint(&cursor, AlgorithmKind::GqlQuery, fp)?;
    Ok(QueryResultWithContinuation {
        result: first_page,
        continuation: Some(token),
    })
}

pub fn explain_gql(gql: String) -> Result<QueryResult, GleaphError> {
    match crate::gql_bridge::explain(&gql) {
        Ok(r) => Ok(r),
        Err(e) => {
            crate::state::increment_rejected_count();
            Err(e)
        }
    }
}

// mutate_gql_with_params removed — merged into mutate_gql(gql, params)

// ── P5 Prepared statements ────────────────────────────────────────────────

pub fn prepare_gql(
    name: String,
    gql: String,
    options: Option<gleaph_types::PreparedOptions>,
) -> Result<gleaph_types::PreparedStatementInfo, GleaphError> {
    crate::gql_bridge::prepare_statement(&name, &gql, options)
}

pub fn execute_prepared_gql(
    name: String,
    params: gleaph_types::PropertyMap,
    sort: Option<Vec<gleaph_types::PreparedSortSpec>>,
) -> Result<QueryResultWithContinuation, GleaphError> {
    let params_map: std::collections::HashMap<String, gleaph_types::Value> =
        params.clone().into_iter().collect();
    let full_result =
        match crate::gql_bridge::execute_prepared_query(&name, &params_map, sort.clone()) {
            Ok(r) => r,
            Err(e) => {
                crate::state::increment_rejected_count();
                return Err(e);
            }
        };
    crate::state::increment_query_count();

    if full_result.rows.len() <= CURSOR_PAGE_SIZE {
        return Ok(QueryResultWithContinuation {
            result: full_result,
            continuation: None,
        });
    }

    let fp = current_graph_fingerprint();
    let first_page = QueryResult {
        columns: full_result.columns.clone(),
        rows: full_result.rows[..CURSOR_PAGE_SIZE].to_vec(),
        stats: full_result.stats.clone(),
        warnings: full_result.warnings.clone(),
    };
    let cursor = GqlQueryCursor {
        query: None,
        prepared_name: Some(name),
        prepared_params: Some(params),
        prepared_sort: sort,
        offset: CURSOR_PAGE_SIZE,
        page_size: CURSOR_PAGE_SIZE,
        total_rows: full_result.rows.len(),
    };
    let token = encode_checkpoint(&cursor, AlgorithmKind::GqlQuery, fp)?;
    Ok(QueryResultWithContinuation {
        result: first_page,
        continuation: Some(token),
    })
}

pub fn execute_prepared_mutation_gql(
    name: String,
    params: gleaph_types::PropertyMap,
) -> Result<MutationResult, GleaphError> {
    let params_map: std::collections::HashMap<String, gleaph_types::Value> =
        params.into_iter().collect();
    if let Err(e) = check_cycle_reserve() {
        crate::state::increment_rejected_count();
        return Err(e);
    }

    let outcome = crate::gql_bridge::execute_prepared_mutation(&name, &params_map);
    match &outcome {
        Ok(o) if !o.affected_vertex_ids.is_empty() => {
            sync_stable_abp_vertex_visibility_incremental_if_needed(&o.affected_vertex_ids)
                .or_else(|_| sync_stable_abp_snapshots_if_needed())?;
            crate::certification::certify_stats(get_stats());
            crate::certification::invalidate_algo_caches();
        }
        Ok(_) => {}
        Err(GleaphError::ParseError(_)) | Err(GleaphError::ValidationError(_)) => {}
        Err(_) => {
            let _ = sync_stable_abp_snapshots_if_needed();
            crate::certification::certify_stats(get_stats());
            crate::certification::invalidate_algo_caches();
        }
    }
    if outcome.is_ok() {
        crate::state::increment_mutation_count();
    } else {
        crate::state::increment_rejected_count();
    }
    let mut result = outcome.map(|o| o.result)?;
    if let Some(warning) = cycle_warning_message() {
        result.warnings.push(gleaph_types::TypeDiagnostic {
            kind: gleaph_types::TypeDiagnosticKind::Info,
            message: warning,
        });
    }
    Ok(result)
}

pub fn drop_prepared_gql(name: String) -> Result<bool, GleaphError> {
    Ok(crate::gql_bridge::drop_prepared(&name))
}

pub fn list_prepared_gql() -> Result<Vec<gleaph_types::PreparedStatementInfo>, GleaphError> {
    Ok(crate::gql_bridge::list_prepared())
}

// batch_mutate_gql_with_params removed — merged into batch_mutate_gql

// mutate_gql_resumable_with_params removed — merged into mutate_gql

/// Executes a GQL mutation with optional parameters.
/// Returns `MutationResultWithContinuation` — large DELETEs may return a continuation token.
pub fn mutate_gql(
    gql: String,
    params: Option<gleaph_types::PropertyMap>,
) -> Result<MutationResultWithContinuation, GleaphError> {
    if let Err(e) = check_cycle_reserve() {
        crate::state::increment_rejected_count();
        return Err(e);
    }

    let progress = if let Some(pm) = params {
        let params_map: std::collections::HashMap<String, gleaph_types::Value> =
            pm.into_iter().collect();
        crate::gql_bridge::mutate_resumable_with_params(
            &gql,
            &params_map,
            MUTATION_CONTINUATION_BUDGET,
        )
    } else {
        crate::gql_bridge::mutate_resumable(&gql, MUTATION_CONTINUATION_BUDGET)
    };
    handle_mutation_progress(progress)
}

pub fn batch_mutate_gql(
    gqls: Vec<(String, Option<gleaph_types::PropertyMap>)>,
) -> Vec<Result<MutationResult, GleaphError>> {
    // Split into GQL strings with optional params for the bridge.
    let gqls_with_maps: Vec<(
        String,
        std::collections::HashMap<String, gleaph_types::Value>,
    )> = gqls
        .into_iter()
        .map(|(gql, pm)| (gql, pm.unwrap_or_default().into_iter().collect()))
        .collect();
    let outcomes = crate::gql_bridge::batch_mutate_tracked_with_params(&gqls_with_maps);

    // Collect all affected vertex IDs across successful outcomes
    let mut all_affected: Vec<u32> = Vec::new();
    let mut any_error_after_write = false;
    for o in &outcomes {
        match o {
            Ok(outcome) => {
                all_affected.extend_from_slice(&outcome.affected_vertex_ids);
                crate::state::increment_mutation_count();
            }
            Err(GleaphError::ParseError(_)) | Err(GleaphError::ValidationError(_)) => {
                crate::state::increment_rejected_count();
            }
            Err(_) => {
                any_error_after_write = true;
                crate::state::increment_rejected_count();
            }
        }
    }

    if any_error_after_write {
        // Fall back to full rebuild on error
        if sync_stable_abp_snapshots_if_needed().is_ok() {
            crate::certification::certify_stats(get_stats());
            crate::certification::invalidate_algo_caches();
        }
    } else if !all_affected.is_empty()
        && sync_stable_abp_vertex_visibility_incremental_if_needed(&all_affected)
            .or_else(|_| sync_stable_abp_snapshots_if_needed())
            .is_ok()
    {
        crate::certification::certify_stats(get_stats());
        crate::certification::invalidate_algo_caches();
    }

    outcomes
        .into_iter()
        .map(|o| o.map(|out| out.result))
        .collect()
}

pub fn create_index(
    entity_type: EntityType,
    property_name: String,
    index_type: IndexType,
) -> Result<(), GleaphError> {
    with_state_mut(|g| g.create_index(entity_type, property_name.clone(), index_type))?;
    if matches!(entity_type, EntityType::Vertex)
        && matches!(index_type, IndexType::Equality | IndexType::Range)
    {
        crate::state::ensure_secondary_index_reserved_region_initialized(0)?;
        crate::state::rebuild_secondary_index_abp_snapshot()?;
    }
    Ok(())
}

pub fn bfs_query(start: u32, config: BfsConfig) -> Result<BfsResultWithContinuation, GleaphError> {
    let fp = current_graph_fingerprint();
    let outcome = with_state(|g| {
        let mut budget = IcBudget::new(5_000_000);
        bfs::bfs_resumable(g, start, &config, &mut budget)
    })?;
    crate::state::increment_algorithm_calls();
    match outcome {
        AlgoOutcome::Done(result) => Ok(BfsResultWithContinuation {
            result,
            continuation: None,
        }),
        AlgoOutcome::Suspended {
            partial,
            checkpoint,
        } => Ok(BfsResultWithContinuation {
            result: partial,
            continuation: Some(encode_checkpoint(&checkpoint, AlgorithmKind::Bfs, fp)?),
        }),
    }
}

pub fn recommend_query(
    user: u32,
    config: RecommendConfig,
) -> Result<Vec<Recommendation>, GleaphError> {
    let result = with_state(|g| {
        let mut budget = IcBudget::new(10_000_000);
        recommend::recommend(g, user, &config, &mut budget)
    });
    if result.is_ok() {
        crate::state::increment_algorithm_calls();
    }
    result
}

pub fn compute_pagerank(
    config: PageRankConfig,
) -> Result<PageRankResultWithContinuation, GleaphError> {
    let fp = current_graph_fingerprint();
    let outcome = with_state(|g| {
        let mut budget = IcBudget::new(40_000_000);
        pagerank::pagerank_resumable(g, &config, &mut budget)
    })?;
    crate::state::increment_algorithm_calls();
    match outcome {
        AlgoOutcome::Done(result) => {
            let key = pagerank_cache_key(&config);
            crate::certification::certify_pagerank(key, result.clone());
            Ok(PageRankResultWithContinuation {
                result,
                continuation: None,
            })
        }
        AlgoOutcome::Suspended {
            partial,
            checkpoint,
        } => Ok(PageRankResultWithContinuation {
            result: partial,
            continuation: Some(encode_checkpoint(&checkpoint, AlgorithmKind::PageRank, fp)?),
        }),
    }
}

pub fn compute_sssp(
    start: u32,
    config: SsspConfig,
) -> Result<SsspResultWithContinuation, GleaphError> {
    let fp = current_graph_fingerprint();
    let outcome = with_state(|g| {
        let mut budget = IcBudget::new(40_000_000);
        sssp::dijkstra_resumable(g, start, &config, &mut budget)
    })?;
    crate::state::increment_algorithm_calls();
    match outcome {
        AlgoOutcome::Done(result) => {
            let key = sssp_cache_key(start, &config);
            crate::certification::cache_sssp_result(key.clone(), &result);
            crate::certification::certify_algo_result(key, &result);
            Ok(SsspResultWithContinuation {
                result,
                continuation: None,
            })
        }
        AlgoOutcome::Suspended {
            partial,
            checkpoint,
        } => Ok(SsspResultWithContinuation {
            result: partial,
            continuation: Some(encode_checkpoint(&checkpoint, AlgorithmKind::Sssp, fp)?),
        }),
    }
}

pub fn get_pagerank_certified(
    config_hash: Vec<u8>,
) -> Result<CertifiedResponse<PageRankResult>, GleaphError> {
    crate::certification::get_pagerank_certified(config_hash).ok_or_else(|| {
        GleaphError::ExecutionError("no cached pagerank result for config_hash".into())
    })
}

// ── Cycle balance monitoring ─────────────────────────────────────────────

/// Minimum cycle reserve below which all mutations are rejected (read-only mode).
#[cfg(target_arch = "wasm32")]
const CYCLE_RESERVE: u128 = 1_000_000_000_000; // 1T
/// Cycle threshold below which a warning is attached to successful mutation results.
#[cfg(target_arch = "wasm32")]
const CYCLE_WARNING: u128 = 5_000_000_000_000; // 5T

/// Returns `Err` when the canister's cycle balance is below the reserve threshold.
/// Always returns `Ok` on non-wasm targets (native tests and benchmarks).
fn check_cycle_reserve() -> Result<(), GleaphError> {
    #[cfg(target_arch = "wasm32")]
    {
        let balance = ic_cdk::api::canister_cycle_balance();
        if balance < CYCLE_RESERVE {
            return Err(GleaphError::ExecutionError(format!(
                "canister cycle reserve depleted (balance: {balance}, min: {CYCLE_RESERVE}); \
                 mutations are suspended until cycles are replenished"
            )));
        }
    }
    Ok(())
}

/// Returns a warning message when balance is low but above the hard reserve.
fn cycle_warning_message() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        let balance = ic_cdk::api::canister_cycle_balance();
        if balance < CYCLE_WARNING {
            return Some(format!(
                "cycle balance low ({balance} < {CYCLE_WARNING}); please add cycles"
            ));
        }
    }
    None
}

// ── ACL helpers ──────────────────────────────────────────────────────────

/// Checks whether the IC caller has at least `required` access level.
/// On non-wasm targets (native tests) this is always permitted.
pub fn check_caller_permission(required: &AccessLevel) -> Result<(), GleaphError> {
    #[cfg(target_arch = "wasm32")]
    {
        let caller = ic_cdk::api::msg_caller();
        let level = crate::state::get_acl_entry(&caller).or_else(|| {
            if ic_cdk::api::is_controller(&caller) {
                Some(AccessLevel::Admin)
            } else if caller == candid::Principal::anonymous() {
                Some(AccessLevel::Read)
            } else {
                None
            }
        });
        let permitted = match (&level, required) {
            (Some(AccessLevel::Admin), _) => true,
            (
                Some(AccessLevel::Write),
                AccessLevel::Read | AccessLevel::Write | AccessLevel::Execute,
            ) => true,
            (Some(AccessLevel::Read), AccessLevel::Read | AccessLevel::Execute) => true,
            (Some(AccessLevel::Execute), AccessLevel::Execute) => true,
            _ => false,
        };
        if !permitted {
            return Err(GleaphError::ExecutionError(format!(
                "permission denied: caller {:?} does not have {:?} access",
                caller, required
            )));
        }
    }
    // On non-wasm targets all callers are implicitly permitted (no IC caller context).
    let _ = required;
    Ok(())
}

/// Sets or updates the ACL entry for a principal. Caller must be Admin.
pub fn set_acl_entry(principal: candid::Principal, level: AccessLevel) -> Result<(), GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    crate::state::set_acl_entry(principal, level);
    Ok(())
}

/// Removes the ACL entry for a principal. Caller must be Admin.
pub fn remove_acl_entry(principal: candid::Principal) -> Result<(), GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    crate::state::remove_acl_entry(&principal);
    Ok(())
}

/// Returns all ACL entries. Caller must have at least Read access.
pub fn list_acl_entries() -> Result<Vec<AclEntry>, GleaphError> {
    check_caller_permission(&AccessLevel::Read)?;
    Ok(crate::state::list_acl_entries())
}

// ── Graph alias (§16.2) Graph alias admin + cross-canister forwarding ────────────────────────

/// Registers or updates a graph alias mapping. Caller must be Admin.
pub fn set_graph_alias(name: String, canister_id: candid::Principal) -> Result<(), GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    if name.is_empty() {
        return Err(GleaphError::ValidationError(
            "graph alias name must not be empty".into(),
        ));
    }
    if name.len() > 128 {
        return Err(GleaphError::ValidationError(
            "graph alias name exceeds 128-byte limit".into(),
        ));
    }
    crate::state::set_graph_alias(name, canister_id);
    Ok(())
}

/// Removes a graph alias. Returns true if the alias existed. Caller must be Admin.
pub fn remove_graph_alias(name: String) -> Result<bool, GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    Ok(crate::state::remove_graph_alias(&name))
}

/// Lists all graph aliases. Caller must have at least Read access.
pub fn list_graph_aliases() -> Result<Vec<GraphAlias>, GleaphError> {
    check_caller_permission(&AccessLevel::Read)?;
    Ok(crate::state::list_graph_aliases())
}

/// Resolves a graph alias name to a canister Principal.
fn resolve_alias(graph_name: &str) -> Result<candid::Principal, GleaphError> {
    crate::state::resolve_graph_alias(graph_name)
        .ok_or_else(|| GleaphError::ExecutionError(format!("unknown graph alias: '{graph_name}'")))
}

/// Forwards a GQL read query to a remote graph canister via inter-canister call.
#[cfg(target_arch = "wasm32")]
pub async fn query_via(
    graph_name: String,
    gql: String,
) -> Result<QueryResultWithContinuation, GleaphError> {
    let target = resolve_alias(&graph_name)?;
    let response = ic_cdk::call::Call::bounded_wait(target, "query")
        .with_args(&(gql,))
        .await
        .map_err(|e| {
            GleaphError::ExecutionError(format!("inter-canister call to {target} failed: {e}"))
        })?;
    let (result,): (Result<QueryResultWithContinuation, GleaphError>,) = response
        .candid_tuple()
        .map_err(|e| GleaphError::ExecutionError(format!("candid decode failed: {e}")))?;
    result
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn query_via(
    graph_name: String,
    _gql: String,
) -> Result<QueryResultWithContinuation, GleaphError> {
    // Validate alias exists even on native, then return unsupported.
    let _ = resolve_alias(&graph_name)?;
    Err(GleaphError::ExecutionError(
        "cross-canister query forwarding is only available on IC (wasm32)".into(),
    ))
}

/// Forwards a GQL mutation to a remote graph canister via inter-canister call.
#[cfg(target_arch = "wasm32")]
pub async fn mutate_via(
    graph_name: String,
    gql: String,
) -> Result<gleaph_types::MutationResult, GleaphError> {
    let target = resolve_alias(&graph_name)?;
    let response = ic_cdk::call::Call::bounded_wait(target, "mutate")
        .with_args(&(gql,))
        .await
        .map_err(|e| {
            GleaphError::ExecutionError(format!("inter-canister call to {target} failed: {e}"))
        })?;
    let (result,): (Result<gleaph_types::MutationResult, GleaphError>,) =
        response
            .candid_tuple()
            .map_err(|e| GleaphError::ExecutionError(format!("candid decode failed: {e}")))?;
    result
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn mutate_via(
    graph_name: String,
    _gql: String,
) -> Result<gleaph_types::MutationResult, GleaphError> {
    let _ = resolve_alias(&graph_name)?;
    Err(GleaphError::ExecutionError(
        "cross-canister mutation forwarding is only available on IC (wasm32)".into(),
    ))
}

// ── Registry delegation (§12) Registry principal + CREATE/DROP GRAPH delegation ────────────────────

/// Sets the registry canister principal used for CREATE/DROP GRAPH delegation.
pub fn set_registry_principal(p: candid::Principal) -> Result<(), GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    crate::state::set_registry_principal(p);
    Ok(())
}

/// Returns the currently configured registry canister principal.
pub fn get_registry_principal() -> Result<Option<candid::Principal>, GleaphError> {
    check_caller_permission(&AccessLevel::Read)?;
    Ok(crate::state::get_registry_principal())
}

fn require_registry_principal() -> Result<candid::Principal, GleaphError> {
    crate::state::get_registry_principal().ok_or_else(|| {
        GleaphError::ExecutionError(
            "registry principal not configured; call set_registry_principal first".into(),
        )
    })
}

/// Creates a graph via the registry canister and auto-registers it as a graph alias.
#[cfg(target_arch = "wasm32")]
pub async fn create_graph_remote(
    name: String,
    config: Option<gleaph_types::GraphConfig>,
) -> Result<gleaph_types::GraphInfo, GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    let registry = require_registry_principal()?;
    let cfg = config.unwrap_or(gleaph_types::GraphConfig {
        name: name.clone(),
        initial_vertex_capacity: 1024,
        initial_edge_capacity: 0,
    });
    let response = ic_cdk::call::Call::bounded_wait(registry, "create_graph")
        .with_args(&(cfg,))
        .await
        .map_err(|e| {
            GleaphError::ExecutionError(format!(
                "inter-canister call to registry {registry} failed: {e}"
            ))
        })?;
    let (result,): (Result<gleaph_types::GraphInfo, String>,) = response
        .candid_tuple()
        .map_err(|e| GleaphError::ExecutionError(format!("candid decode failed: {e}")))?;
    let info = result.map_err(GleaphError::ExecutionError)?;
    // Auto-register the new canister as a graph alias.
    if let Some(cid) = info.canister_id {
        crate::state::set_graph_alias(name, cid);
    }
    Ok(info)
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn create_graph_remote(
    _name: String,
    _config: Option<gleaph_types::GraphConfig>,
) -> Result<gleaph_types::GraphInfo, GleaphError> {
    let _ = require_registry_principal()?;
    Err(GleaphError::ExecutionError(
        "CREATE GRAPH delegation is only available on IC (wasm32)".into(),
    ))
}

/// Drops a graph via the registry canister and removes its graph alias.
#[cfg(target_arch = "wasm32")]
pub async fn drop_graph_remote(name: String) -> Result<bool, GleaphError> {
    check_caller_permission(&AccessLevel::Admin)?;
    let registry = require_registry_principal()?;
    // List graphs from registry to find the ID by name.
    let response = ic_cdk::call::Call::bounded_wait(registry, "list_graphs")
        .await
        .map_err(|e| {
            GleaphError::ExecutionError(format!(
                "inter-canister call to registry {registry} failed: {e}"
            ))
        })?;
    let (graphs,): (Vec<gleaph_types::GraphInfo>,) = response
        .candid_tuple()
        .map_err(|e| GleaphError::ExecutionError(format!("candid decode failed: {e}")))?;
    let graph = graphs.iter().find(|g| g.name == name).ok_or_else(|| {
        GleaphError::ExecutionError(format!("no graph named '{name}' found in registry"))
    })?;
    let graph_id = graph.id;
    let response = ic_cdk::call::Call::bounded_wait(registry, "delete_graph")
        .with_args(&(graph_id,))
        .await
        .map_err(|e| {
            GleaphError::ExecutionError(format!(
                "inter-canister call to registry {registry} failed: {e}"
            ))
        })?;
    let (deleted,): (bool,) = response
        .candid_tuple()
        .map_err(|e| GleaphError::ExecutionError(format!("candid decode failed: {e}")))?;
    if deleted {
        crate::state::remove_graph_alias(&name);
    }
    Ok(deleted)
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn drop_graph_remote(_name: String) -> Result<bool, GleaphError> {
    let _ = require_registry_principal()?;
    Err(GleaphError::ExecutionError(
        "DROP GRAPH delegation is only available on IC (wasm32)".into(),
    ))
}

// ── Unified async GQL endpoint (D1 + D2) ──────────────────────────────────────

/// Extracts the right-hand GQL substring from `USE GRAPH <name> NEXT <rest>`.
///
/// Finds the `NEXT` keyword after the graph name and returns everything after it.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn extract_rhs_after_next(gql: &str) -> Option<String> {
    // Tokenize: "USE GRAPH name NEXT <rest>"
    // Find NEXT keyword (case-insensitive) after USE GRAPH <name>.
    let upper = gql.to_ascii_uppercase();
    let next_pos = upper.find("NEXT")?;
    let after_next = &gql[next_pos + 4..];
    let trimmed = after_next.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Unified async GQL endpoint that handles all statement kinds:
/// - `USE GRAPH name NEXT stmt` → transparent routing to target canister (D1)
/// - `USE GRAPH name` (standalone) → alias resolution
/// - `CREATE GRAPH name` → delegate to registry
/// - `DROP GRAPH name` → delegate to registry
/// - Other read queries → local query execution
/// - Other mutations → local mutation execution
#[cfg(target_arch = "wasm32")]
pub async fn execute_gql(gql: String) -> Result<ExecuteGqlResult, GleaphError> {
    use gleaph_gql::ast::{SetOp, Statement};

    crate::gql_bridge::enforce_limits(&gql)?;
    let stmt = gleaph_gql::parse_statement(&gql)?;
    gleaph_gql::validate_statement(&stmt)?;

    match stmt {
        // D1: USE GRAPH name NEXT stmt → route to target canister.
        Statement::Compound {
            op: SetOp::Next(_),
            left,
            right: _,
        } if matches!(*left, Statement::UseGraph(_)) => {
            let Statement::UseGraph(ref graph_name) = *left else {
                unreachable!()
            };
            let target = resolve_alias(graph_name)?;
            let rhs_gql = extract_rhs_after_next(&gql).ok_or_else(|| {
                GleaphError::ExecutionError("USE GRAPH NEXT requires a statement after NEXT".into())
            })?;
            // Try as query first, then as mutation.
            let response = ic_cdk::call::Call::bounded_wait(target, "query")
                .with_args(&(rhs_gql.clone(),))
                .await
                .map_err(|e| {
                    GleaphError::ExecutionError(format!(
                        "inter-canister call to {target} failed: {e}"
                    ))
                })?;
            let (query_result,): (Result<QueryResultWithContinuation, GleaphError>,) = response
                .candid_tuple()
                .map_err(|e| GleaphError::ExecutionError(format!("candid decode failed: {e}")))?;
            match query_result {
                Ok(r) => Ok(ExecuteGqlResult::Query(r)),
                Err(_query_err) => {
                    // Query failed (possibly a mutation) — try mutation endpoint.
                    let response = ic_cdk::call::Call::bounded_wait(target, "mutate")
                        .with_args(&(rhs_gql,))
                        .await
                        .map_err(|e| {
                            GleaphError::ExecutionError(format!(
                                "inter-canister call to {target} failed: {e}"
                            ))
                        })?;
                    let (mutate_result,): (Result<MutationResult, GleaphError>,) =
                        response.candid_tuple().map_err(|e| {
                            GleaphError::ExecutionError(format!("candid decode failed: {e}"))
                        })?;
                    Ok(ExecuteGqlResult::Mutation(mutate_result?))
                }
            }
        }

        // Standalone USE GRAPH → alias resolution (informational).
        Statement::UseGraph(ref name) => {
            let result = crate::gql_bridge::resolve_use_graph_public(name)?;
            Ok(ExecuteGqlResult::Query(QueryResultWithContinuation {
                result,
                continuation: None,
            }))
        }

        // D2: CREATE GRAPH → delegate to registry.
        Statement::CreateGraph { ref name, .. } => {
            check_caller_permission(&AccessLevel::Admin)?;
            let info = create_graph_remote(name.clone(), None).await?;
            Ok(ExecuteGqlResult::GraphCreated(info))
        }

        // D2: DROP GRAPH → delegate to registry.
        Statement::DropGraph { ref name, .. } => {
            check_caller_permission(&AccessLevel::Admin)?;
            let dropped = drop_graph_remote(name.clone()).await?;
            Ok(ExecuteGqlResult::GraphDropped(dropped))
        }

        // Read queries → local execution.
        _ if is_read_statement(&stmt) => {
            check_caller_permission(&AccessLevel::Read)?;
            let result = query_gql(gql, None)?;
            Ok(ExecuteGqlResult::Query(result))
        }

        // Mutations → local execution.
        _ => {
            check_caller_permission(&AccessLevel::Write)?;
            let result = mutate_gql(gql, None)?;
            Ok(ExecuteGqlResult::Mutation(result.result))
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn execute_gql(gql: String) -> Result<ExecuteGqlResult, GleaphError> {
    use gleaph_gql::ast::{SetOp, Statement};

    crate::gql_bridge::enforce_limits(&gql)?;
    let stmt = gleaph_gql::parse_statement(&gql)?;
    gleaph_gql::validate_statement(&stmt)?;

    match stmt {
        Statement::Compound {
            op: SetOp::Next(_),
            left,
            right: _,
        } if matches!(*left, Statement::UseGraph(_)) => {
            let Statement::UseGraph(ref graph_name) = *left else {
                unreachable!()
            };
            let _ = resolve_alias(graph_name)?;
            Err(GleaphError::ExecutionError(
                "USE GRAPH NEXT routing is only available on IC (wasm32)".into(),
            ))
        }

        Statement::UseGraph(ref name) => {
            let result = crate::gql_bridge::resolve_use_graph_public(name)?;
            Ok(ExecuteGqlResult::Query(QueryResultWithContinuation {
                result,
                continuation: None,
            }))
        }

        Statement::CreateGraph { ref name, .. } => {
            let info = create_graph_remote(name.clone(), None).await?;
            Ok(ExecuteGqlResult::GraphCreated(info))
        }

        Statement::DropGraph { ref name, .. } => {
            let dropped = drop_graph_remote(name.clone()).await?;
            Ok(ExecuteGqlResult::GraphDropped(dropped))
        }

        _ if is_read_statement(&stmt) => {
            let result = query_gql(gql, None)?;
            Ok(ExecuteGqlResult::Query(result))
        }

        _ => {
            let result = mutate_gql(gql, None)?;
            Ok(ExecuteGqlResult::Mutation(result.result))
        }
    }
}

/// Returns true if the statement is a read-only (query) statement.
fn is_read_statement(stmt: &gleaph_gql::ast::Statement) -> bool {
    use gleaph_gql::ast::Statement;
    matches!(
        stmt,
        Statement::Query(_) | Statement::Compound { .. } | Statement::UseGraph(_)
    )
}

// ── Resumable algorithm endpoints ────────────────────────────────────────

const MAX_CHECKPOINT_SIZE: usize = 1_500_000;

fn current_graph_fingerprint() -> GraphFingerprint {
    with_state(|g| GraphFingerprint {
        num_vertices: g.vertex_count(),
        num_edges: g.edge_count(),
        next_edge_id: g.next_edge_id,
    })
}

fn validate_fingerprint(fp: &GraphFingerprint) -> Result<(), GleaphError> {
    let current = current_graph_fingerprint();
    if *fp != current {
        return Err(GleaphError::AlgorithmError(
            "graph modified since checkpoint; continuation token is invalid".into(),
        ));
    }
    Ok(())
}

fn encode_checkpoint<T: serde::Serialize>(
    checkpoint: &T,
    kind: AlgorithmKind,
    fp: GraphFingerprint,
) -> Result<ContinuationToken, GleaphError> {
    let data = serde_cbor::to_vec(checkpoint)
        .map_err(|e| GleaphError::AlgorithmError(format!("checkpoint serialize: {e}")))?;
    if data.len() > MAX_CHECKPOINT_SIZE {
        return Err(GleaphError::AlgorithmError(format!(
            "algorithm state too large for continuation ({} bytes > {} limit); \
             consider reducing max_visited or max_distance",
            data.len(),
            MAX_CHECKPOINT_SIZE
        )));
    }
    Ok(ContinuationToken {
        kind,
        data,
        graph_fingerprint: fp,
    })
}

// bfs_query_resumable, compute_sssp_resumable, compute_pagerank_resumable
// removed — merged into bfs_query, compute_sssp, compute_pagerank

// ── GQL query cursor ────────────────────────────────────────────────────

/// Internal cursor state for GQL query pagination (CBOR-serialized in ContinuationToken).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct GqlQueryCursor {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    prepared_name: Option<String>,
    #[serde(default)]
    prepared_params: Option<gleaph_types::PropertyMap>,
    #[serde(default)]
    prepared_sort: Option<Vec<gleaph_types::PreparedSortSpec>>,
    offset: usize,
    page_size: usize,
    total_rows: usize,
}

/// Default page size for cursor-based query pagination.
const CURSOR_PAGE_SIZE: usize = 1_000;

// query_gql_resumable removed — query_gql now always supports continuation

// ── Resumable mutation endpoints ─────────────────────────────────────────

/// Step budget per continuation round for resumable mutations.
const MUTATION_CONTINUATION_BUDGET: u64 = 500;

// mutate_gql_resumable removed — merged into mutate_gql

/// Resumes a suspended mutation from a continuation token.
pub fn mutate_continue(
    token: ContinuationToken,
) -> Result<MutationResultWithContinuation, GleaphError> {
    if token.kind != AlgorithmKind::Mutation {
        return Err(GleaphError::AlgorithmError(format!(
            "expected Mutation continuation token, got {:?}",
            token.kind
        )));
    }
    // No fingerprint validation: the graph is expected to change between mutation rounds.
    let checkpoint: gleaph_types::MutationCheckpoint = serde_cbor::from_slice(&token.data)
        .map_err(|e| GleaphError::AlgorithmError(format!("invalid mutation checkpoint: {e}")))?;

    let progress = crate::gql_bridge::resume_mutation(checkpoint, MUTATION_CONTINUATION_BUDGET);
    handle_mutation_progress(progress)
}

/// Common handler for mutation progress: syncs indices, certifies, and encodes continuation.
fn handle_mutation_progress(
    progress: Result<gleaph_gql::executor::MutationProgress, GleaphError>,
) -> Result<MutationResultWithContinuation, GleaphError> {
    use gleaph_gql::executor::MutationProgress;

    match progress {
        Ok(MutationProgress::Done(outcome)) => {
            if !outcome.affected_vertex_ids.is_empty() {
                sync_stable_abp_vertex_visibility_incremental_if_needed(
                    &outcome.affected_vertex_ids,
                )
                .or_else(|_| sync_stable_abp_snapshots_if_needed())?;
                crate::certification::certify_stats(get_stats());
                crate::certification::invalidate_algo_caches();
            }
            crate::state::increment_mutation_count();
            let mut result = outcome.result;
            if let Some(warning) = cycle_warning_message() {
                result.warnings.push(gleaph_types::TypeDiagnostic {
                    kind: gleaph_types::TypeDiagnosticKind::Info,
                    message: warning,
                });
            }
            Ok(MutationResultWithContinuation {
                result,
                continuation: None,
            })
        }
        Ok(MutationProgress::Suspended {
            partial,
            checkpoint,
        }) => {
            // Sync partial progress
            if !partial.affected_vertex_ids.is_empty() {
                let _ = sync_stable_abp_vertex_visibility_incremental_if_needed(
                    &partial.affected_vertex_ids,
                )
                .or_else(|_| sync_stable_abp_snapshots_if_needed());
                crate::certification::certify_stats(get_stats());
                crate::certification::invalidate_algo_caches();
            }
            let fp = current_graph_fingerprint();
            let token = encode_checkpoint(&checkpoint, AlgorithmKind::Mutation, fp)?;
            Ok(MutationResultWithContinuation {
                result: partial.result,
                continuation: Some(token),
            })
        }
        Err(e @ (GleaphError::ParseError(_) | GleaphError::ValidationError(_))) => {
            crate::state::increment_rejected_count();
            Err(e)
        }
        Err(e) => {
            let _ = sync_stable_abp_snapshots_if_needed();
            crate::certification::certify_stats(get_stats());
            crate::certification::invalidate_algo_caches();
            crate::state::increment_rejected_count();
            Err(e)
        }
    }
}

// ── Continuation dispatcher ──────────────────────────────────────

pub fn query_continue(token: ContinuationToken) -> Result<ContinuationResult, GleaphError> {
    validate_fingerprint(&token.graph_fingerprint)?;
    let fp = token.graph_fingerprint.clone();

    match token.kind {
        AlgorithmKind::Bfs => {
            let checkpoint: bfs::BfsCheckpoint = serde_cbor::from_slice(&token.data)
                .map_err(|e| GleaphError::AlgorithmError(format!("invalid checkpoint: {e}")))?;
            let outcome = with_state(|g| {
                let mut budget = IcBudget::new(5_000_000);
                bfs::bfs_resume(g, checkpoint, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();
            match outcome {
                AlgoOutcome::Done(result) => {
                    Ok(ContinuationResult::Bfs(BfsResultWithContinuation {
                        result,
                        continuation: None,
                    }))
                }
                AlgoOutcome::Suspended {
                    partial,
                    checkpoint,
                } => Ok(ContinuationResult::Bfs(BfsResultWithContinuation {
                    result: partial,
                    continuation: Some(encode_checkpoint(&checkpoint, AlgorithmKind::Bfs, fp)?),
                })),
            }
        }
        AlgorithmKind::Sssp => {
            let checkpoint: sssp::SsspCheckpoint = serde_cbor::from_slice(&token.data)
                .map_err(|e| GleaphError::AlgorithmError(format!("invalid checkpoint: {e}")))?;
            let outcome = with_state(|g| {
                let mut budget = IcBudget::new(40_000_000);
                sssp::dijkstra_resume(g, checkpoint, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();
            match outcome {
                AlgoOutcome::Done(result) => {
                    Ok(ContinuationResult::Sssp(SsspResultWithContinuation {
                        result,
                        continuation: None,
                    }))
                }
                AlgoOutcome::Suspended {
                    partial,
                    checkpoint,
                } => Ok(ContinuationResult::Sssp(SsspResultWithContinuation {
                    result: partial,
                    continuation: Some(encode_checkpoint(&checkpoint, AlgorithmKind::Sssp, fp)?),
                })),
            }
        }
        AlgorithmKind::PageRank => {
            let checkpoint: pagerank::PageRankCheckpoint = serde_cbor::from_slice(&token.data)
                .map_err(|e| GleaphError::AlgorithmError(format!("invalid checkpoint: {e}")))?;
            let outcome = with_state(|g| {
                let mut budget = IcBudget::new(40_000_000);
                pagerank::pagerank_resume(g, checkpoint, &mut budget)
            })?;
            crate::state::increment_algorithm_calls();
            match outcome {
                AlgoOutcome::Done(result) => Ok(ContinuationResult::PageRank(
                    PageRankResultWithContinuation {
                        result,
                        continuation: None,
                    },
                )),
                AlgoOutcome::Suspended {
                    partial,
                    checkpoint,
                } => Ok(ContinuationResult::PageRank(
                    PageRankResultWithContinuation {
                        result: partial,
                        continuation: Some(encode_checkpoint(
                            &checkpoint,
                            AlgorithmKind::PageRank,
                            fp,
                        )?),
                    },
                )),
            }
        }
        AlgorithmKind::GqlQuery => {
            let cursor: GqlQueryCursor = serde_cbor::from_slice(&token.data)
                .map_err(|e| GleaphError::AlgorithmError(format!("invalid cursor: {e}")))?;

            let full_result = if let Some(name) = &cursor.prepared_name {
                let params_map: std::collections::HashMap<String, gleaph_types::Value> = cursor
                    .prepared_params
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                crate::gql_bridge::execute_prepared_query(
                    name,
                    &params_map,
                    cursor.prepared_sort.clone(),
                )?
            } else {
                let query = cursor.query.as_deref().ok_or_else(|| {
                    GleaphError::AlgorithmError("query cursor missing query source".into())
                })?;
                // Re-execute query (fingerprint already validated → deterministic results)
                crate::gql_bridge::query_paged(query)?
            };
            crate::state::increment_query_count();

            let start = cursor.offset.min(full_result.rows.len());
            let end = (cursor.offset + cursor.page_size).min(full_result.rows.len());
            let page_rows = full_result.rows[start..end].to_vec();

            let continuation = if end < full_result.rows.len() {
                let next_cursor = GqlQueryCursor {
                    query: cursor.query,
                    prepared_name: cursor.prepared_name,
                    prepared_params: cursor.prepared_params,
                    prepared_sort: cursor.prepared_sort,
                    offset: end,
                    page_size: cursor.page_size,
                    total_rows: full_result.rows.len(),
                };
                Some(encode_checkpoint(
                    &next_cursor,
                    AlgorithmKind::GqlQuery,
                    fp,
                )?)
            } else {
                None
            };

            Ok(ContinuationResult::Query(QueryResultWithContinuation {
                result: QueryResult {
                    columns: full_result.columns,
                    rows: page_rows,
                    stats: full_result.stats,
                    warnings: full_result.warnings,
                },
                continuation,
            }))
        }
        AlgorithmKind::Mutation => Err(GleaphError::AlgorithmError(
            "mutation continuation tokens must be resumed via mutate_continue, not query_continue"
                .into(),
        )),
    }
}

fn pagerank_cache_key(config: &PageRankConfig) -> Vec<u8> {
    encode_one(config).unwrap_or_default()
}

fn sssp_cache_key(start: u32, config: &SsspConfig) -> Vec<u8> {
    let mut key = start.to_le_bytes().to_vec();
    key.extend(encode_one(config).unwrap_or_default());
    key
}

#[cfg(test)]
fn mutation_changes_graph(result: &MutationResult) -> bool {
    result.affected_vertices > 0 || result.affected_edges > 0
}

#[cfg(test)]
fn mutation_result_may_have_changed_graph(result: &Result<MutationResult, GleaphError>) -> bool {
    match result {
        Ok(result) => mutation_changes_graph(result),
        // `gql_bridge::mutate` parses/validates before entering `with_state_mut`, so these
        // variants are guaranteed to occur before any graph mutation happens.
        Err(GleaphError::ParseError(_)) | Err(GleaphError::ValidationError(_)) => false,
        // GQL mutations are not transactional; if execution returns an error, earlier writes may
        // already be committed. Re-certify/invalidate conservatively to avoid serving stale
        // certified stats or cached algorithm results.
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state;

    #[test]
    fn legacy_add_edge_revives_gql_deleted_edge() {
        state::init_state(4, 0).expect("init state");
        add_vertex(gleaph_types::VertexData { id: 0 }).expect("v0");
        add_vertex(gleaph_types::VertexData { id: 1 }).expect("v1");
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
                .expect("seed labeled edge");
            g.delete_edge(0, 1, Some("KNOWS")).expect("tombstone edge");
        });

        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 42,
        })
        .expect("legacy add_edge should revive endpoint pair visibility");

        let neighbors = get_neighbors(0);
        assert!(neighbors.iter().any(|e| e.target == 1));
        state::with_state(|g| {
            assert!(g.edge_matches_label(0, 1, "KNOWS"));
            let edge = g.edge_record(0, 1, Some("KNOWS")).expect("edge record");
            assert!(edge.props.is_empty());
        });
    }

    #[test]
    fn legacy_add_vertex_revives_gql_deleted_vertex() {
        state::init_state(4, 0).expect("init state");
        add_vertex(gleaph_types::VertexData { id: 0 }).expect("v0");
        add_vertex(gleaph_types::VertexData { id: 1 }).expect("v1");
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
                .expect("seed edge");
            g.delete_vertex(1).expect("tombstone vertex");
        });

        assert!(
            get_neighbors(0).is_empty(),
            "deleted vertex should be hidden"
        );

        add_vertex(gleaph_types::VertexData { id: 1 })
            .expect("legacy add_vertex should revive endpoint visibility");

        let neighbors = get_neighbors(0);
        assert!(neighbors.iter().any(|e| e.target == 1));
    }

    #[test]
    fn create_index_allocates_reserved_secondary_index_region_metadata() {
        state::init_state(8, 0).expect("init state");

        create_index(EntityType::Vertex, "uid".to_string(), IndexType::Equality)
            .expect("create vertex eq index");

        let (_prop_off, _prop_len, sec_off, sec_len, non_pma_base) =
            state::current_regions_meta().expect("regions meta");
        assert!(
            sec_len > 0,
            "secondary index reserved length should be allocated"
        );
        assert!(
            sec_off >= non_pma_base,
            "secondary index must be above non_pma_base"
        );
        assert!(
            non_pma_base > 0,
            "non_pma_base should be initialized on first non-PMA allocation"
        );
        state::with_state(|g| {
            let hdr = gleaph_pma::abp_tree::AbpStoreHeader::read_from(&g.mem, sec_off)
                .expect("ABP secondary index header should be initialized");
            assert!(hdr.next_page_id >= 1);
        });
    }

    #[test]
    fn mutate_gql_rebuilds_stable_secondary_index_snapshot_when_reserved() {
        use gleaph_types::Value;

        state::init_state(16, 0).expect("init state");
        create_index(EntityType::Vertex, "uid".to_string(), IndexType::Equality)
            .expect("create index");

        mutate_gql(r#"INSERT (:User {uid: 11, name: 'A'})"#.into(), None)
            .expect("create user with uid");

        let (_po, _pl, sec_off, _sl, _base) = state::current_regions_meta().expect("regions");
        state::with_state(|g| {
            let hits_11 = g
                .scan_vertices_by_property_eq_abp(g.mem.clone(), sec_off, "uid", &Value::Int32(11))
                .expect("scan stable idx new");
            assert_eq!(hits_11.len(), 1);
        });
    }

    #[test]
    fn mutate_gql_auto_allocates_and_rebuilds_property_store_snapshot_when_props_exist() {
        state::init_state(16, 0).expect("init state");

        mutate_gql(r#"INSERT (:User {name: 'A', age: 20})"#.into(), None)
            .expect("create with props");

        let (prop_off, prop_len, _sec_off, _sec_len, non_pma_base) =
            state::current_regions_meta().expect("regions");
        assert!(prop_off > 0);
        assert!(prop_len > 0);
        assert!(prop_off >= non_pma_base);
        state::with_state(|g| {
            let hdr = gleaph_pma::abp_tree::AbpStoreHeader::read_from(&g.mem, prop_off)
                .expect("ABP property-store header should be initialized");
            assert!(hdr.next_page_id >= 1);
        });
    }

    #[test]
    fn legacy_add_vertex_rehydrates_stable_property_and_secondary_snapshots_on_revival() {
        use gleaph_pma::AbpPropertyStore;
        use gleaph_types::Value;

        state::init_state(16, 0).expect("init state");
        create_index(EntityType::Vertex, "uid".to_string(), IndexType::Equality)
            .expect("create vertex eq index");
        mutate_gql(r#"INSERT (:User {uid: 11, name: 'A'})"#.into(), None).expect("create user");
        let vid = state::with_state(|g| {
            (0..g.vertex_count() as u32)
                .find(|&v| {
                    g.get_vertex_props(v)
                        .map(|ps| {
                            ps.iter()
                                .any(|(k, val)| k == "uid" && *val == Value::Int32(11))
                        })
                        .unwrap_or(false)
                })
                .expect("find created user vertex id")
        });
        state::with_state_mut(|g| g.delete_vertex(vid).expect("delete user"));
        crate::state::rebuild_property_store_abp_snapshot()
            .expect("rebuild property snapshot after delete");
        crate::state::rebuild_secondary_index_abp_snapshot()
            .expect("rebuild secondary snapshot after delete");

        add_vertex(gleaph_types::VertexData { id: vid }).expect("revive via legacy add_vertex");

        let (prop_off, _prop_len, sec_off, _sec_len, _base) =
            state::current_regions_meta().expect("regions");
        state::with_state(|g| {
            let store = AbpPropertyStore::from_memory(g.mem.clone(), prop_off)
                .expect("open stable property store");
            assert_eq!(store.get_vertex_prop(vid, "uid"), Some(Value::Int32(11)));
            assert_eq!(
                store.get_vertex_prop(vid, "name"),
                Some(Value::Text("A".into()))
            );

            let hits = g
                .scan_vertices_by_property_eq_abp(g.mem.clone(), sec_off, "uid", &Value::Int32(11))
                .expect("scan stable idx");
            assert_eq!(hits, VertexIdSet::from_iter([vid]));
        });
    }

    #[test]
    fn legacy_add_edge_preserves_gql_edge_properties_on_revival() {
        state::init_state(4, 0).expect("init state");
        add_vertex(gleaph_types::VertexData { id: 0 }).expect("v0");
        add_vertex(gleaph_types::VertexData { id: 1 }).expect("v1");
        state::with_state_mut(|g| {
            g.create_edge(
                0,
                1,
                Some("KNOWS".into()),
                vec![("since".into(), gleaph_types::Value::Int64(2020))],
                1.0,
                1,
            )
            .expect("seed labeled edge");
            g.delete_edge(0, 1, Some("KNOWS")).expect("tombstone edge");
        });

        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 42,
        })
        .expect("legacy add_edge should revive metadata");

        state::with_state(|g| {
            assert!(g.edge_matches_label(0, 1, "KNOWS"));
            let edge = g.edge_record(0, 1, Some("KNOWS")).expect("edge record");
            assert_eq!(
                edge.props,
                vec![("since".into(), gleaph_types::Value::Int64(2020))]
            );
        });
    }

    #[test]
    fn legacy_add_edge_rehydrates_stable_property_snapshot_for_revived_edge_props() {
        use gleaph_pma::AbpPropertyStore;
        use gleaph_types::Value;

        state::init_state(8, 0).expect("init state");
        mutate_gql(r#"INSERT (:A {idv: 1})"#.into(), None).expect("create a");
        mutate_gql(r#"INSERT (:B {idv: 2})"#.into(), None).expect("create b");
        state::with_state_mut(|g| {
            g.create_edge(
                0,
                1,
                Some("KNOWS".into()),
                vec![("since".into(), Value::Int64(2020))],
                1.0,
                1,
            )
            .expect("seed labeled edge");
        });
        crate::state::ensure_property_store_reserved_region_initialized(0)
            .expect("reserve+init property region");
        crate::state::rebuild_property_store_abp_snapshot().expect("initial property snapshot");
        state::with_state_mut(|g| g.delete_edge(0, 1, Some("KNOWS")).expect("tombstone edge"));
        crate::state::rebuild_property_store_abp_snapshot().expect("snapshot after delete");

        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 7.0,
            timestamp: 99,
        })
        .expect("legacy revive edge");

        let (prop_off, _pl, _so, _sl, _base) = state::current_regions_meta().expect("regions");
        state::with_state(|g| {
            let store = AbpPropertyStore::from_memory(g.mem.clone(), prop_off).expect("open store");
            let edge_id = g.edge_id_for_labeled(0, 1, Some("KNOWS"));
            assert_eq!(
                store.get_edge_prop_by_id(edge_id, "since"),
                Some(Value::Int64(2020))
            );
        });
    }

    #[test]
    fn bulk_insert_edges_rehydrates_stable_property_snapshot_for_revived_edge_props() {
        use gleaph_pma::AbpPropertyStore;
        use gleaph_types::Value;

        state::init_state(8, 0).expect("init state");
        mutate_gql(r#"INSERT (:A {idv: 1})"#.into(), None).expect("create a");
        mutate_gql(r#"INSERT (:B {idv: 2})"#.into(), None).expect("create b");
        state::with_state_mut(|g| {
            g.create_edge(
                0,
                1,
                Some("KNOWS".into()),
                vec![("since".into(), Value::Int64(2021))],
                1.0,
                1,
            )
            .expect("seed labeled edge");
        });
        crate::state::ensure_property_store_reserved_region_initialized(0)
            .expect("reserve+init property region");
        crate::state::rebuild_property_store_abp_snapshot().expect("initial property snapshot");
        state::with_state_mut(|g| g.delete_edge(0, 1, Some("KNOWS")).expect("tombstone edge"));
        crate::state::rebuild_property_store_abp_snapshot().expect("snapshot after delete");

        bulk_insert_edges(vec![EdgeData {
            src: 0,
            dst: 1,
            weight: 8.0,
            timestamp: 100,
        }])
        .expect("legacy bulk revive edge");

        let (prop_off, _pl, _so, _sl, _base) = state::current_regions_meta().expect("regions");
        state::with_state(|g| {
            let store = AbpPropertyStore::from_memory(g.mem.clone(), prop_off).expect("open store");
            let edge_id = g.edge_id_for_labeled(0, 1, Some("KNOWS"));
            assert_eq!(
                store.get_edge_prop_by_id(edge_id, "since"),
                Some(Value::Int64(2021))
            );
        });
    }

    #[test]
    fn legacy_add_edge_updates_payload_on_gql_revival() {
        state::init_state(4, 0).expect("init state");
        add_vertex(gleaph_types::VertexData { id: 0 }).expect("v0");
        add_vertex(gleaph_types::VertexData { id: 1 }).expect("v1");
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
                .expect("seed edge");
            g.delete_edge(0, 1, Some("KNOWS")).expect("tombstone edge");
        });

        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 7.5,
            timestamp: 1234,
        })
        .expect("legacy add_edge should update payload when reviving");

        let neighbors = get_neighbors(0);
        let edge = neighbors
            .into_iter()
            .find(|e| e.target == 1)
            .expect("revived edge visible");
        assert_eq!(edge.weight, 7.5);
        assert_eq!(edge.timestamp, 1234);
    }

    #[test]
    fn legacy_add_edge_revives_gql_deleted_vertices() {
        state::init_state(4, 0).expect("init state");
        add_vertex(gleaph_types::VertexData { id: 0 }).expect("v0");
        add_vertex(gleaph_types::VertexData { id: 1 }).expect("v1");
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
                .expect("seed edge");
            g.delete_vertex(1).expect("tombstone vertex");
        });

        assert!(
            get_neighbors(0).is_empty(),
            "tombstoned vertex should be hidden"
        );

        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 99,
        })
        .expect("legacy add_edge should revive endpoints");

        state::with_state(|g| {
            assert!(!g.is_vertex_tombstoned(0));
            assert!(!g.is_vertex_tombstoned(1));
        });
        assert!(get_neighbors(0).iter().any(|e| e.target == 1));
    }

    #[test]
    fn legacy_add_vertex_preserves_gql_vertex_properties_on_revival() {
        state::init_state(4, 0).expect("init state");
        add_vertex(gleaph_types::VertexData { id: 1 }).expect("seed vertex");
        state::with_state_mut(|g| {
            g.set_vertex_props(
                1,
                vec![("name".into(), gleaph_types::Value::Text("A".into()))],
            )
            .expect("set props");
            g.delete_vertex(1).expect("tombstone vertex");
        });

        add_vertex(gleaph_types::VertexData { id: 1 }).expect("legacy revive vertex");

        state::with_state(|g| {
            assert_eq!(
                g.get_vertex_props(1),
                Some(vec![("name".into(), gleaph_types::Value::Text("A".into()))])
            );
        });
    }

    #[test]
    fn query_gql_rejects_overlong_query_before_execution() {
        let gql = "M".repeat(16 * 1024 + 1);
        let err = query_gql(gql, None).expect_err("should reject query longer than hard cap");
        assert!(matches!(err, GleaphError::UnsupportedFeature(_)));
        assert!(err.to_string().contains("query too long"));
    }

    #[test]
    fn query_gql_rejects_limit_overflow_via_parser() {
        let err = query_gql(
            "MATCH (a)-[:X]->(b) RETURN a LIMIT 5000000000".to_string(),
            None,
        )
        .expect_err("should reject LIMIT larger than u32");
        assert!(matches!(err, GleaphError::ParseError(_)));
        assert!(err.to_string().contains("LIMIT exceeds"));
    }

    #[test]
    fn query_gql_auto_pages_when_row_cap_exceeded_with_real_graph_data() {
        state::init_state(256, 1_024).expect("init state");
        state::with_state_mut(|g| {
            let mut ids = Vec::new();
            for _ in 0..11 {
                ids.push(
                    g.create_vertex(vec!["User".into()], Vec::new())
                        .expect("vertex"),
                );
            }
            for &src in &ids {
                for &dst in &ids {
                    g.create_edge(src, dst, Some("KNOWS".into()), Vec::new(), 1.0, 1)
                        .expect("edge");
                }
            }
        });

        // large result sets are now auto-paged instead of rejected
        let res = query_gql(
            "MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c)-[:KNOWS]->(d) RETURN d.id".to_string(),
            None,
        )
        .expect("should auto-page large result set");
        assert_eq!(
            res.result.rows.len(),
            1000,
            "first page should be capped at page size"
        );
        assert!(res.continuation.is_some(), "should have continuation token");
    }

    #[test]
    fn query_gql_returns_error_when_execution_step_cap_exceeded_with_real_graph_data() {
        state::init_state(256, 1_024).expect("init state");
        state::with_state_mut(|g| {
            let mut ids = Vec::new();
            for _ in 0..11 {
                ids.push(
                    g.create_vertex(vec!["User".into()], Vec::new())
                        .expect("vertex"),
                );
            }
            for &src in &ids {
                for &dst in &ids {
                    g.create_edge(src, dst, Some("KNOWS".into()), Vec::new(), 1.0, 1)
                        .expect("edge");
                }
            }
        });

        let projection = std::iter::repeat_n("a.id", 1_201)
            .collect::<Vec<_>>()
            .join(", ");
        let gql = format!(
            "MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c)-[:KNOWS]->(d) RETURN {projection} LIMIT 1000"
        );
        let err = query_gql(gql, None).expect_err("should reject queries above execution step cap");
        assert!(matches!(err, GleaphError::ExecutionError(_)));
        assert!(err.to_string().contains("execution steps"));
    }

    #[test]
    fn no_op_gql_mutations_do_not_invalidate_pagerank_cache() {
        use gleaph_algo::pagerank::PageRankConfig;

        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");
        add_edge(EdgeData {
            src: 2,
            dst: 0,
            weight: 1.0,
            timestamp: 3,
        })
        .expect("e3");

        let cfg = PageRankConfig {
            damping: 0.85,
            max_iterations: 5,
            convergence_threshold: 1e-6,
            ts_range: None,
        };
        let cached_before = compute_pagerank(cfg.clone())
            .expect("compute pagerank")
            .result;
        let cfg_hash = pagerank_cache_key(&cfg);
        assert_eq!(
            get_pagerank_certified(cfg_hash.clone())
                .expect("pagerank cache before no-op mutate")
                .data,
            cached_before
        );

        let no_op = mutate_gql(
            "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.id = 999999 DELETE b".into(),
            None,
        )
        .expect("no-op mutate should succeed");
        assert_eq!(no_op.result.affected_vertices, 0);
        assert_eq!(no_op.result.affected_edges, 0);
        assert_eq!(
            get_pagerank_certified(cfg_hash.clone())
                .expect("pagerank cache should remain after no-op mutate")
                .data,
            cached_before
        );

        let batch = batch_mutate_gql(vec![(
            "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.id = 999999 DELETE b".into(),
            None,
        )]);
        let batch_no_op = batch
            .into_iter()
            .next()
            .expect("single result")
            .expect("batch no-op mutate should succeed");
        assert_eq!(batch_no_op.affected_vertices, 0);
        assert_eq!(batch_no_op.affected_edges, 0);
        assert_eq!(
            get_pagerank_certified(cfg_hash)
                .expect("pagerank cache should remain after no-op batch mutate")
                .data,
            cached_before
        );
    }

    #[test]
    fn no_op_gql_mutations_preserve_certified_stats_response() {
        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");

        let before = get_stats_certified();
        assert!(before.data.num_edges >= 2);

        let no_op = mutate_gql(
            "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.id = 999999 DELETE b".into(),
            None,
        )
        .expect("no-op mutate should succeed");
        assert_eq!(no_op.result.affected_vertices, 0);
        assert_eq!(no_op.result.affected_edges, 0);
        let after_single = get_stats_certified();
        assert_eq!(after_single.data, before.data);
        assert_eq!(after_single.witness, before.witness);

        let batch = batch_mutate_gql(vec![(
            "MATCH (a:User)-[:KNOWS]->(b:User) WHERE b.id = 999999 DELETE b".into(),
            None,
        )]);
        let batch_no_op = batch
            .into_iter()
            .next()
            .expect("single result")
            .expect("batch no-op mutate should succeed");
        assert_eq!(batch_no_op.affected_vertices, 0);
        assert_eq!(batch_no_op.affected_edges, 0);

        let after_batch = get_stats_certified();
        assert_eq!(after_batch.data, before.data);
        assert_eq!(after_batch.witness, before.witness);
    }

    #[test]
    fn mutation_invalidation_predicate_is_conservative_on_errors() {
        assert!(!mutation_result_may_have_changed_graph(&Err(
            GleaphError::ParseError("bad syntax".into()),
        )));
        assert!(!mutation_result_may_have_changed_graph(&Err(
            GleaphError::ValidationError("bad shape".into()),
        )));

        assert!(!mutation_result_may_have_changed_graph(&Ok(
            MutationResult {
                affected_vertices: 0,
                affected_edges: 0,
                warnings: Vec::new(),
            }
        )));

        assert!(mutation_result_may_have_changed_graph(&Ok(
            MutationResult {
                affected_vertices: 1,
                affected_edges: 0,
                warnings: Vec::new(),
            }
        )));

        assert!(mutation_result_may_have_changed_graph(&Err(
            GleaphError::ExecutionError(
                "non-transactional mutation may have partially written".into()
            ),
        )));
    }

    #[test]
    fn batch_invalidation_predicate_treats_error_items_as_potential_writes() {
        let batch = [
            Ok(MutationResult {
                affected_vertices: 0,
                affected_edges: 0,
                warnings: Vec::new(),
            }),
            Err(GleaphError::ExecutionError("partial failure".into())),
        ];

        assert!(batch.iter().any(mutation_result_may_have_changed_graph));
    }

    #[test]
    fn mutate_gql_recertifies_stats_when_error_occurs_after_state_write() {
        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");

        let before = get_stats_certified();
        assert_eq!(before.data.num_vertices, 0);

        crate::gql_bridge::arm_test_fail_after_mutation_once();
        let err = mutate_gql(r#"INSERT (:User {name: 'A'})"#.into(), None)
            .expect_err("failpoint should convert committed mutation into error");
        assert!(matches!(err, GleaphError::ExecutionError(_)));

        let current = get_stats();
        let after = get_stats_certified();
        assert_eq!(
            current, after.data,
            "certified stats must track committed state"
        );
        assert!(after.data.num_vertices > before.data.num_vertices);
    }

    #[test]
    fn batch_mutate_gql_recertifies_stats_when_item_errors_after_state_write() {
        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");

        let before = get_stats_certified();

        crate::gql_bridge::arm_test_fail_after_mutation_once();
        let results = batch_mutate_gql(vec![(r#"INSERT (:User {name: 'A'})"#.into(), None)]);
        assert_eq!(results.len(), 1);
        let err = results.into_iter().next().expect("one result").unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));

        let current = get_stats();
        let after = get_stats_certified();
        assert_eq!(
            current, after.data,
            "certified stats must track committed state"
        );
        assert!(after.data.num_vertices > before.data.num_vertices);
    }

    #[test]
    fn mutate_gql_invalidates_pagerank_cache_when_error_occurs_after_state_write() {
        use gleaph_algo::pagerank::PageRankConfig;

        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");
        add_edge(EdgeData {
            src: 2,
            dst: 0,
            weight: 1.0,
            timestamp: 3,
        })
        .expect("e3");

        let cfg = PageRankConfig {
            damping: 0.85,
            max_iterations: 5,
            convergence_threshold: 1e-6,
            ts_range: None,
        };
        let _ = compute_pagerank(cfg.clone()).expect("seed pagerank cache");
        let cfg_hash = pagerank_cache_key(&cfg);
        assert!(
            get_pagerank_certified(cfg_hash.clone()).is_ok(),
            "pagerank cache should exist before mutation"
        );

        crate::gql_bridge::arm_test_fail_after_mutation_once();
        let err = mutate_gql(r#"INSERT (:User {name: 'B'})"#.into(), None)
            .expect_err("failpoint should convert committed mutation into error");
        assert!(matches!(err, GleaphError::ExecutionError(_)));

        let after = get_pagerank_certified(cfg_hash).expect_err(
            "pagerank cache should be invalidated when graph changes even if mutation returns Err",
        );
        assert!(matches!(after, GleaphError::ExecutionError(_)));
    }

    #[test]
    fn batch_mutate_gql_invalidates_pagerank_cache_when_item_errors_after_state_write() {
        use gleaph_algo::pagerank::PageRankConfig;

        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");
        add_edge(EdgeData {
            src: 2,
            dst: 0,
            weight: 1.0,
            timestamp: 3,
        })
        .expect("e3");

        let cfg = PageRankConfig {
            damping: 0.85,
            max_iterations: 5,
            convergence_threshold: 1e-6,
            ts_range: None,
        };
        let _ = compute_pagerank(cfg.clone()).expect("seed pagerank cache");
        let cfg_hash = pagerank_cache_key(&cfg);
        assert!(
            get_pagerank_certified(cfg_hash.clone()).is_ok(),
            "pagerank cache should exist before batch mutation"
        );

        crate::gql_bridge::arm_test_fail_after_mutation_once();
        let results = batch_mutate_gql(vec![(r#"INSERT (:User {name: 'B'})"#.into(), None)]);
        assert_eq!(results.len(), 1);
        let err = results.into_iter().next().expect("one result").unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));

        let after = get_pagerank_certified(cfg_hash).expect_err(
            "pagerank cache should be invalidated when batch mutation commits then errors",
        );
        assert!(matches!(after, GleaphError::ExecutionError(_)));
    }

    #[test]
    fn mutate_gql_invalidates_sssp_cache_when_error_occurs_after_state_write() {
        use gleaph_algo::sssp::SsspConfig;

        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");

        let cfg = SsspConfig::default();
        let _ = compute_sssp(0, cfg.clone()).expect("seed sssp cache");
        let key = sssp_cache_key(0, &cfg);
        let caches_before = crate::certification::snapshot_caches();
        assert!(
            caches_before.sssp_cache.iter().any(|(k, _)| *k == key),
            "sssp cache should exist before mutation"
        );

        crate::gql_bridge::arm_test_fail_after_mutation_once();
        let err = mutate_gql(r#"INSERT (:User {name: 'B'})"#.into(), None)
            .expect_err("failpoint should convert committed mutation into error");
        assert!(matches!(err, GleaphError::ExecutionError(_)));

        let caches_after = crate::certification::snapshot_caches();
        assert!(
            caches_after.sssp_cache.iter().all(|(k, _)| *k != key),
            "sssp cache should be invalidated when graph changes even if mutation returns Err"
        );
    }

    #[test]
    fn batch_mutate_gql_invalidates_sssp_cache_when_item_errors_after_state_write() {
        use gleaph_algo::sssp::SsspConfig;

        crate::certification::init_certification();
        state::init_state(16, 0).expect("init state");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");

        let cfg = SsspConfig::default();
        let _ = compute_sssp(0, cfg.clone()).expect("seed sssp cache");
        let key = sssp_cache_key(0, &cfg);
        let caches_before = crate::certification::snapshot_caches();
        assert!(
            caches_before.sssp_cache.iter().any(|(k, _)| *k == key),
            "sssp cache should exist before batch mutation"
        );

        crate::gql_bridge::arm_test_fail_after_mutation_once();
        let results = batch_mutate_gql(vec![(r#"INSERT (:User {name: 'B'})"#.into(), None)]);
        assert_eq!(results.len(), 1);
        let err = results.into_iter().next().expect("one result").unwrap_err();
        assert!(matches!(err, GleaphError::ExecutionError(_)));

        let caches_after = crate::certification::snapshot_caches();
        assert!(
            caches_after.sssp_cache.iter().all(|(k, _)| *k != key),
            "sssp cache should be invalidated when batch mutation commits then errors"
        );
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn pagerank_certified_cache_persists_across_state_restore() {
        use gleaph_algo::pagerank::PageRankConfig;

        crate::state::init_state(64, 0).expect("init");
        add_edge(EdgeData {
            src: 0,
            dst: 1,
            weight: 1.0,
            timestamp: 1,
        })
        .expect("e1");
        add_edge(EdgeData {
            src: 1,
            dst: 2,
            weight: 1.0,
            timestamp: 2,
        })
        .expect("e2");
        add_edge(EdgeData {
            src: 2,
            dst: 0,
            weight: 1.0,
            timestamp: 3,
        })
        .expect("e3");

        let cfg = PageRankConfig {
            damping: 0.85,
            max_iterations: 5,
            convergence_threshold: 1e-6,
            ts_range: None,
        };
        let before = compute_pagerank(cfg.clone()).expect("compute");
        let cfg_hash = pagerank_cache_key(&cfg);
        let cached_before = get_pagerank_certified(cfg_hash.clone()).expect("cache before");
        assert_eq!(cached_before.data, before);

        crate::state::persist_state_metadata().expect("persist");
        crate::state::restore_state().expect("restore");
        crate::certification::certify_stats(get_stats());

        let cached_after = get_pagerank_certified(cfg_hash).expect("cache after restore");
        assert_eq!(cached_after.data, before);
    }

    #[test]
    fn query_gql_supports_union_compound_queries() {
        state::init_state(16, 0).expect("init state");
        mutate_gql(r#"INSERT (:User {name: 'Alice'})"#.into(), None).expect("create alice");
        mutate_gql(r#"INSERT (:Admin {name: 'Bob'})"#.into(), None).expect("create bob");

        // UNION deduplicates
        let res = query_gql(
            "MATCH (a:User)-[:X]->(b) RETURN a.name UNION MATCH (c:Admin)-[:Y]->(d) RETURN c.name"
                .into(),
            None,
        )
        .expect("union query");
        assert_eq!(res.result.columns.len(), 1);
        // Both branches should return results (Alice from User, Bob from Admin)
        // No edges exist so both branches return empty — that's fine, verifies no crash.
        // The important thing is compound queries don't error out anymore.
        assert!(res.result.rows.len() <= 2);
    }

    #[test]
    fn query_gql_supports_union_all_with_results() {
        use gleaph_types::Value;

        state::init_state(16, 0).expect("init state");
        let v0 = state::with_state_mut(|g| {
            g.create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Alice".into()))],
            )
            .expect("v0")
        });
        let v1 = state::with_state_mut(|g| {
            g.create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Bob".into()))],
            )
            .expect("v1")
        });
        state::with_state_mut(|g| {
            g.create_edge(v0, v1, Some("KNOWS".into()), Vec::new(), 1.0, 0)
                .expect("edge")
        });

        // UNION ALL preserves duplicates
        let res = query_gql(
            "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name UNION ALL MATCH (c:User)-[:KNOWS]->(d:User) RETURN c.name"
                .into(),
            None,
        )
        .expect("union all query");
        assert_eq!(res.result.columns.len(), 1);
        // Both branches return Alice → Bob, so "Alice" appears twice with UNION ALL
        assert_eq!(res.result.rows.len(), 2);
        assert_eq!(res.result.rows[0][0], Value::Text("Alice".into()));
        assert_eq!(res.result.rows[1][0], Value::Text("Alice".into()));
    }

    #[test]
    fn query_gql_supports_except_compound_queries() {
        use gleaph_types::Value;

        state::init_state(16, 0).expect("init state");
        let v0 = state::with_state_mut(|g| {
            g.create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Alice".into()))],
            )
            .expect("v0")
        });
        let v1 = state::with_state_mut(|g| {
            g.create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("Bob".into()))],
            )
            .expect("v1")
        });
        state::with_state_mut(|g| {
            g.create_edge(v0, v1, Some("KNOWS".into()), Vec::new(), 1.0, 0)
                .expect("edge0-1")
        });
        state::with_state_mut(|g| {
            g.create_edge(v1, v0, Some("KNOWS".into()), Vec::new(), 1.0, 0)
                .expect("edge1-0")
        });

        // EXCEPT: all KNOWS sources minus those where target is Alice
        let res = query_gql(
            "MATCH (a:User)-[:KNOWS]->(b:User) RETURN a.name EXCEPT MATCH (c:User)-[:KNOWS]->(d:User) WHERE d.name = 'Alice' RETURN c.name"
                .into(),
            None,
        )
        .expect("except query");
        assert_eq!(res.result.columns.len(), 1);
        // Left: Alice, Bob; Right: Bob (→Alice). Except removes Bob, leaves Alice.
        assert_eq!(res.result.rows.len(), 1);
        assert_eq!(res.result.rows[0][0], Value::Text("Alice".into()));
    }

    // ── Canary deployment endpoint ─────────────────────────────────────

    #[test]
    fn canister_info_returns_current_version() {
        let g = crate::state::init_state(8, 0);
        assert!(g.is_ok(), "init_state should succeed");
        let info = get_canister_info();
        assert_eq!(
            info.layout_version,
            gleaph_types::STABLE_VERSION,
            "layout_version should match STABLE_VERSION"
        );
    }

    // ── GQL query cursor tests ─────────────────────────────────────────

    #[test]
    fn query_cursor_returns_all_rows_across_pages() {
        use gleaph_types::ContinuationResult;
        state::init_state(128, 0).expect("init state");
        // Create 35 Person vertices — WITH cross join produces 35*34 = 1190 rows (> CURSOR_PAGE_SIZE=1000)
        for i in 0..35u32 {
            mutate_gql(format!(r#"INSERT (:Person {{name: 'P{i}'}})"#), None)
                .expect("create person");
        }

        // Use WITH to create a cross-product: MATCH a, WITH a, MATCH b, WHERE a<>b
        let gql = "MATCH (a:Person) WITH a MATCH (b:Person) WHERE a <> b RETURN a.name, b.name";
        let page1 = query_gql(gql.into(), None).expect("page 1");
        assert_eq!(page1.result.rows.len(), 1000);
        assert!(
            page1.continuation.is_some(),
            "should have continuation token"
        );
        assert_eq!(page1.result.columns, vec!["a.name", "b.name"]);

        // Second call via query_continue: should return remaining 190 rows with no continuation
        let token = page1.continuation.unwrap();
        let page2_result = query_continue(token).expect("page 2");
        let page2 = match page2_result {
            ContinuationResult::Query(q) => q,
            other => panic!("expected Query continuation, got {other:?}"),
        };
        assert_eq!(page2.result.rows.len(), 190);
        assert!(page2.continuation.is_none(), "should have no more pages");

        // Verify combined row count
        let total = page1.result.rows.len() + page2.result.rows.len();
        assert_eq!(total, 35 * 34, "should have all cross-join rows");
    }

    #[test]
    fn cursor_preserves_order_by() {
        use gleaph_types::{ContinuationResult, Value};
        state::init_state(128, 0).expect("init state");
        // Create 35 Person vertices with age property
        for i in 0..35u32 {
            mutate_gql(format!(r#"INSERT (:Person {{age: {i}}})"#), None).expect("create person");
        }

        // ORDER BY ensures deterministic cross-page ordering
        let gql = "MATCH (a:Person) WITH a MATCH (b:Person) WHERE a <> b RETURN a.age, b.age ORDER BY a.age, b.age";
        let page1 = query_gql(gql.into(), None).expect("page 1");
        assert_eq!(page1.result.rows.len(), 1000);
        assert!(page1.continuation.is_some());

        let token = page1.continuation.unwrap();
        let page2 = match query_continue(token).expect("page 2") {
            ContinuationResult::Query(q) => q,
            other => panic!("expected Query, got {other:?}"),
        };
        assert_eq!(page2.result.rows.len(), 190);

        // Concatenate all rows and verify sorted order
        let mut all_rows = page1.result.rows;
        all_rows.extend(page2.result.rows);
        assert_eq!(all_rows.len(), 1190);

        for window in all_rows.windows(2) {
            let (a_age_0, b_age_0) = match (&window[0][0], &window[0][1]) {
                (Value::Int32(a), Value::Int32(b)) => (*a as i64, *b as i64),
                _ => panic!("expected Int values"),
            };
            let (a_age_1, b_age_1) = match (&window[1][0], &window[1][1]) {
                (Value::Int32(a), Value::Int32(b)) => (*a as i64, *b as i64),
                _ => panic!("expected Int values"),
            };
            assert!(
                (a_age_0, b_age_0) <= (a_age_1, b_age_1),
                "rows should be sorted: ({a_age_0},{b_age_0}) <= ({a_age_1},{b_age_1})"
            );
        }
    }

    // ── Auto cursor fallback tests ──────────────────────────────────────

    #[test]
    fn auto_fallback_activates_for_heavy_query() {
        use gleaph_types::ContinuationResult;
        state::init_state(128, 0).expect("init state");
        // Create 35 Person vertices — cross join produces 35*34 = 1190 rows (> 1000 page size)
        for i in 0..35u32 {
            mutate_gql(format!(r#"INSERT (:Person {{name: 'P{i}'}})"#), None).expect("create");
        }

        // query_gql (the `query` endpoint) should auto-page large results
        let gql = "MATCH (a:Person) WITH a MATCH (b:Person) WHERE a <> b RETURN a.name, b.name";
        let page1 = query_gql(gql.into(), None).expect("auto fallback page 1");
        assert_eq!(
            page1.result.rows.len(),
            1000,
            "first page should have 1000 rows"
        );
        assert!(
            page1.continuation.is_some(),
            "should have continuation token for auto-fallback"
        );

        // Continue with the cursor token
        let token = page1.continuation.unwrap();
        let page2 = match query_continue(token).expect("page 2") {
            ContinuationResult::Query(q) => q,
            other => panic!("expected Query continuation, got {other:?}"),
        };
        assert_eq!(page2.result.rows.len(), 190);
        assert!(page2.continuation.is_none(), "no more pages");
        assert_eq!(
            page1.result.rows.len() + page2.result.rows.len(),
            1190,
            "total rows should match cross-join"
        );
    }

    #[test]
    fn light_query_uses_direct_path() {
        state::init_state(16, 0).expect("init state");
        mutate_gql(r#"INSERT (:User {name: 'Alice'})"#.into(), None).expect("create");
        mutate_gql(r#"INSERT (:User {name: 'Bob'})"#.into(), None).expect("create");

        // Small query — should return all results with no continuation
        let res = query_gql("MATCH (n:User) RETURN n.name ORDER BY n.name".into(), None)
            .expect("light query");
        assert_eq!(res.result.rows.len(), 2);
        assert!(
            res.continuation.is_none(),
            "no continuation for small result set"
        );
        assert_eq!(res.result.columns, vec!["n.name"]);
    }

    // ── Resumable mutation tests ─────────────────────────────────────────

    #[test]
    fn heavy_mutation_completes_via_continuation() {
        state::init_state(128, 0).expect("init state");
        // Create 60 Person pairs connected by KNOWS edges
        for i in 0..30 {
            mutate_gql(
                format!(
                    r#"INSERT (:Person {{idx: {}}})-[:KNOWS]->(:Person {{idx: {}}})"#,
                    i * 2,
                    i * 2 + 1
                ),
                None,
            )
            .expect("create person pair");
        }

        // DETACH DELETE all Person vertices matched via the hop pattern
        let first = mutate_gql(
            "MATCH (n:Person)-[e:KNOWS]->(m:Person) DETACH DELETE n".into(),
            None,
        )
        .expect("first mutation round");

        let mut total_v = first.result.affected_vertices;
        let mut total_e = first.result.affected_edges;
        let mut token = first.continuation;

        while let Some(t) = token {
            let cont = mutate_continue(t).expect("continue mutation");
            total_v += cont.result.affected_vertices;
            total_e += cont.result.affected_edges;
            token = cont.continuation;
        }

        // Each CREATE makes 2 vertices + 1 edge = 60 vertices + 30 edges.
        // DETACH DELETE n deletes the source vertices (30 unique) + their incident edges (30).
        assert!(total_v > 0, "should have deleted some vertices");
        assert!(total_e > 0, "should have deleted some edges");
    }

    #[test]
    fn heavy_mutation_suspends_with_small_budget() {
        state::init_state(128, 0).expect("init state");
        // Create 30 pairs connected by LINK edges
        for i in 0..30 {
            mutate_gql(
                format!(
                    r#"INSERT (:Item {{idx: {}}})-[:LINK]->(:Item {{idx: {}}})"#,
                    i * 2,
                    i * 2 + 1
                ),
                None,
            )
            .expect("create item pair");
        }

        // Use mutate_resumable with a small budget via gql_bridge directly.
        // Budget must be large enough for the MATCH phase to complete (which counts its own
        // execution steps), but small enough that the delete phase suspends.
        // With 30 pairs (60 vertices + 30 edges), MATCH needs ~100 steps to scan,
        // so budget=10 for the delete phase should cause suspension.
        let progress = crate::gql_bridge::mutate_resumable(
            "MATCH (n:Item)-[e:LINK]->(m:Item) DETACH DELETE n",
            10, // small budget — causes suspension during delete phase
        )
        .expect("resumable mutation");

        match progress {
            gleaph_gql::executor::MutationProgress::Suspended {
                partial,
                checkpoint,
            } => {
                let ops = partial.result.affected_vertices + partial.result.affected_edges;
                assert_eq!(ops, 10, "10 operations in first round (budget=10)");
                match checkpoint {
                    gleaph_types::MutationCheckpoint::Delete(dc) => {
                        let remaining = dc.remaining_vertices.len() + dc.remaining_edges.len();
                        assert!(remaining > 0, "checkpoint has remaining work");
                    }
                    _ => panic!("expected Delete checkpoint"),
                }
            }
            gleaph_gql::executor::MutationProgress::Done(_) => {
                panic!("expected suspension with budget=10 and 30 pairs");
            }
        }
    }

    #[test]
    fn light_mutation_has_no_continuation() {
        state::init_state(16, 0).expect("init state");
        // Create 3 pairs — small enough to finish within budget
        for i in 0..3 {
            mutate_gql(
                format!(
                    r#"INSERT (:Small {{idx: {}}})-[:REL]->(:Small {{idx: {}}})"#,
                    i * 2,
                    i * 2 + 1
                ),
                None,
            )
            .expect("create");
        }

        let res = mutate_gql(
            "MATCH (n:Small)-[e:REL]->(m:Small) DETACH DELETE n".into(),
            None,
        )
        .expect("light mutation");
        assert!(res.result.affected_vertices > 0);
        assert!(res.continuation.is_none(), "under budget — no continuation");
    }

    #[test]
    fn mutate_continue_rejects_query_token() {
        state::init_state(8, 0).expect("init state");
        let token = ContinuationToken {
            kind: AlgorithmKind::GqlQuery,
            data: vec![],
            graph_fingerprint: current_graph_fingerprint(),
        };
        let err = mutate_continue(token).expect_err("should reject query token");
        assert!(err.to_string().contains("expected Mutation"));
    }

    #[test]
    fn query_continue_rejects_mutation_token() {
        state::init_state(8, 0).expect("init state");
        let token = ContinuationToken {
            kind: AlgorithmKind::Mutation,
            data: vec![],
            graph_fingerprint: current_graph_fingerprint(),
        };
        let err = query_continue(token).expect_err("should reject mutation token");
        assert!(err.to_string().contains("mutate_continue"));
    }

    // ── Graph alias (§16.2) Graph alias tests ────────────────────────────────────────────────

    #[test]
    fn use_graph_resolves_alias_in_query() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        crate::state::set_graph_alias("remote".into(), pid);

        let result = query_gql("USE GRAPH remote".into(), None).expect("USE GRAPH should succeed");
        assert_eq!(result.result.columns, vec!["graph_name", "canister_id"]);
        assert_eq!(result.result.rows.len(), 1);
        assert_eq!(
            result.result.rows[0][0],
            gleaph_types::Value::Text("remote".into())
        );
        assert_eq!(
            result.result.rows[0][1],
            gleaph_types::Value::Text(pid.to_text())
        );
    }

    #[test]
    fn use_graph_unknown_alias_returns_error() {
        state::init_state(4, 0).expect("init state");
        let err = query_gql("USE GRAPH nonexistent".into(), None)
            .expect_err("should fail for unknown alias");
        assert!(err.to_string().contains("unknown graph alias"));
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn use_graph_in_mutation_context_rejected() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        crate::state::set_graph_alias("remote".into(), pid);

        let err = mutate_gql("USE GRAPH remote".into(), None)
            .expect_err("USE GRAPH via mutate should be rejected");
        assert!(err.to_string().contains("read-only operation"));
    }

    #[test]
    fn graph_alias_crud_operations() {
        state::init_state(4, 0).expect("init state");
        let pid1 = candid::Principal::from_text("aaaaa-aa").unwrap();
        let pid2 = candid::Principal::from_text("2vxsx-fae").unwrap();

        // Set
        set_graph_alias("g1".into(), pid1).expect("set g1");
        set_graph_alias("g2".into(), pid2).expect("set g2");

        // List
        let aliases = list_graph_aliases().expect("list");
        assert_eq!(aliases.len(), 2);

        // Resolve
        assert_eq!(crate::state::resolve_graph_alias("g1"), Some(pid1));
        assert_eq!(crate::state::resolve_graph_alias("g2"), Some(pid2));

        // Update existing
        set_graph_alias("g1".into(), pid2).expect("update g1");
        assert_eq!(crate::state::resolve_graph_alias("g1"), Some(pid2));

        // Remove
        let removed = remove_graph_alias("g1".into()).expect("remove g1");
        assert!(removed);
        assert_eq!(crate::state::resolve_graph_alias("g1"), None);

        // Remove non-existent
        let removed = remove_graph_alias("g1".into()).expect("remove g1 again");
        assert!(!removed);
    }

    #[test]
    fn graph_alias_empty_name_rejected() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        let err = set_graph_alias("".into(), pid).expect_err("empty name should be rejected");
        assert!(err.to_string().contains("must not be empty"));
    }

    /// Drive a synchronously-completing async fn to completion (no real IO in non-wasm32 stubs).
    fn block_on_sync<F: std::future::Future>(f: F) -> F::Output {
        let mut f = std::pin::pin!(f);
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        match f.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("unexpected Pending from sync-returning async fn"),
        }
    }

    #[test]
    fn query_via_native_returns_unsupported() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        crate::state::set_graph_alias("remote".into(), pid);

        let err = block_on_sync(query_via("remote".into(), "MATCH (n) RETURN n".into()))
            .expect_err("should return unsupported on native");
        assert!(err.to_string().contains("only available on IC"));
    }

    #[test]
    fn mutate_via_native_returns_unsupported() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        crate::state::set_graph_alias("remote".into(), pid);

        let err = block_on_sync(mutate_via("remote".into(), "INSERT (:Test)".into()))
            .expect_err("should return unsupported on native");
        assert!(err.to_string().contains("only available on IC"));
    }

    // ── Registry delegation (§12) CREATE/DROP GRAPH + registry principal tests ─────────────────────

    #[test]
    fn create_graph_in_query_context_rejected() {
        state::init_state(4, 0).expect("init state");
        let err = query_gql("CREATE GRAPH mydb".into(), None)
            .expect_err("CREATE GRAPH via query should be rejected");
        assert!(err.to_string().contains("execute_gql"));
    }

    #[test]
    fn drop_graph_in_query_context_rejected() {
        state::init_state(4, 0).expect("init state");
        let err = query_gql("DROP GRAPH mydb".into(), None)
            .expect_err("DROP GRAPH via query should be rejected");
        assert!(err.to_string().contains("execute_gql"));
    }

    #[test]
    fn create_graph_in_mutate_context_returns_error() {
        state::init_state(4, 0).expect("init state");
        let err = mutate_gql("CREATE GRAPH mydb".into(), None)
            .expect_err("CREATE GRAPH via mutate should return error");
        assert!(err.to_string().contains("execute_gql"));
    }

    #[test]
    fn drop_graph_in_mutate_context_returns_error() {
        state::init_state(4, 0).expect("init state");
        let err = mutate_gql("DROP GRAPH mydb".into(), None)
            .expect_err("DROP GRAPH via mutate should return error");
        assert!(err.to_string().contains("execute_gql"));
    }

    #[test]
    fn registry_principal_crud() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        // Initially none
        assert_eq!(get_registry_principal().unwrap(), None);
        // Set
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        set_registry_principal(pid).expect("set should succeed");
        assert_eq!(get_registry_principal().unwrap(), Some(pid));
        // Update
        let pid2 = candid::Principal::from_text("2vxsx-fae").unwrap();
        set_registry_principal(pid2).expect("update should succeed");
        assert_eq!(get_registry_principal().unwrap(), Some(pid2));
        // Clean up
        crate::state::reset_metrics_and_quota_for_test();
    }

    #[test]
    fn create_graph_remote_native_returns_unsupported() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        set_registry_principal(pid).expect("set registry");

        let err = block_on_sync(create_graph_remote("test".into(), None))
            .expect_err("should return unsupported on native");
        assert!(err.to_string().contains("only available on IC"));
        crate::state::reset_metrics_and_quota_for_test();
    }

    #[test]
    fn drop_graph_remote_native_returns_unsupported() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        set_registry_principal(pid).expect("set registry");

        let err = block_on_sync(drop_graph_remote("test".into()))
            .expect_err("should return unsupported on native");
        assert!(err.to_string().contains("only available on IC"));
        crate::state::reset_metrics_and_quota_for_test();
    }

    #[test]
    fn create_graph_remote_without_registry_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        // No registry principal set
        let err = block_on_sync(create_graph_remote("test".into(), None))
            .expect_err("should fail without registry");
        assert!(
            err.to_string()
                .contains("registry principal not configured")
        );
        crate::state::reset_metrics_and_quota_for_test();
    }

    #[test]
    fn drop_graph_remote_without_registry_returns_error() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        // No registry principal set
        let err = block_on_sync(drop_graph_remote("test".into()))
            .expect_err("should fail without registry");
        assert!(
            err.to_string()
                .contains("registry principal not configured")
        );
        crate::state::reset_metrics_and_quota_for_test();
    }

    // ── D1/D2 execute_gql tests ────────────────────────────────────────────────

    #[test]
    fn extract_rhs_after_next_works() {
        assert_eq!(
            extract_rhs_after_next("USE GRAPH foo NEXT MATCH (n) RETURN n"),
            Some("MATCH (n) RETURN n".to_string())
        );
        assert_eq!(
            extract_rhs_after_next("use graph foo next match (n) return n"),
            Some("match (n) return n".to_string())
        );
        assert_eq!(extract_rhs_after_next("USE GRAPH foo"), None);
        assert_eq!(extract_rhs_after_next("MATCH (n) RETURN n"), None);
    }

    #[test]
    fn execute_gql_use_graph_standalone() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        crate::state::set_graph_alias("remote".into(), pid);

        let result = block_on_sync(execute_gql("USE GRAPH remote".into()))
            .expect("USE GRAPH should succeed");
        match result {
            ExecuteGqlResult::Query(qr) => {
                assert_eq!(qr.result.columns, vec!["graph_name", "canister_id"]);
            }
            _ => panic!("expected Query variant"),
        }
    }

    #[test]
    fn execute_gql_use_graph_next_native_unsupported() {
        state::init_state(4, 0).expect("init state");
        let pid = candid::Principal::from_text("aaaaa-aa").unwrap();
        crate::state::set_graph_alias("remote".into(), pid);

        let err = block_on_sync(execute_gql(
            "USE GRAPH remote NEXT MATCH (n) RETURN n".into(),
        ))
        .expect_err("USE GRAPH NEXT should fail on native");
        assert!(err.to_string().contains("only available on IC"));
    }

    #[test]
    fn execute_gql_local_query() {
        state::init_state(4, 0).expect("init state");
        let result = block_on_sync(execute_gql("MATCH (n) RETURN n".into()))
            .expect("local query should succeed");
        assert!(matches!(result, ExecuteGqlResult::Query(_)));
    }

    #[test]
    fn execute_gql_create_graph_without_registry() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        let err = block_on_sync(execute_gql("CREATE GRAPH testdb".into()))
            .expect_err("should fail without registry");
        assert!(
            err.to_string()
                .contains("registry principal not configured")
        );
        crate::state::reset_metrics_and_quota_for_test();
    }

    #[test]
    fn execute_gql_drop_graph_without_registry() {
        crate::state::reset_metrics_and_quota_for_test();
        state::init_state(4, 0).expect("init state");
        let err = block_on_sync(execute_gql("DROP GRAPH testdb".into()))
            .expect_err("should fail without registry");
        assert!(
            err.to_string()
                .contains("registry principal not configured")
        );
        crate::state::reset_metrics_and_quota_for_test();
    }

    // ── GQL-integrated DDL tests ─────────────────────────────────────────────

    #[test]
    fn show_stats_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        mutate_gql("INSERT (:User {name: 'Alice'})".into(), None).unwrap();
        let res = query_gql("SHOW STATS".into(), None).unwrap();
        let r = res.result;
        assert_eq!(
            r.columns,
            vec![
                "num_vertices",
                "num_edges",
                "elem_capacity",
                "segment_size",
                "segment_count",
                "avg_degree"
            ]
        );
        assert_eq!(r.rows.len(), 1);
        // num_vertices should be positive (exact count depends on test ordering)
        assert!(matches!(r.rows[0][0], gleaph_types::Value::Int64(v) if v > 0));
    }

    #[test]
    fn show_indexes_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        create_index(EntityType::Vertex, "name".into(), IndexType::Equality).unwrap();
        let res = query_gql("SHOW INDEXES".into(), None).unwrap();
        let r = res.result;
        assert_eq!(
            r.columns,
            vec!["entity_type", "property_name", "index_type"]
        );
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][1], gleaph_types::Value::Text("name".into()));
    }

    #[test]
    fn show_schemas_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        mutate_gql("CREATE SCHEMA test_schema".into(), None).unwrap();
        let res = query_gql("SHOW SCHEMAS".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["schema_name"]);
        assert!(
            r.rows
                .iter()
                .any(|row| row[0] == gleaph_types::Value::Text("test_schema".into()))
        );
    }

    #[test]
    fn show_quota_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        let res = query_gql("SHOW QUOTA".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["max_vertices", "max_edges"]);
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn show_prepared_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        prepare_gql(
            "find_user".into(),
            "MATCH (u:User) RETURN u.name".into(),
            None,
        )
        .unwrap();
        let res = query_gql("SHOW PREPARED".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["name", "source"]);
        assert!(
            r.rows
                .iter()
                .any(|row| row[0] == gleaph_types::Value::Text("find_user".into()))
        );
    }

    #[test]
    fn show_planner_stats_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        let res = query_gql("SHOW PLANNER STATS".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["key", "value", "float_value"]);
        // At minimum there should be a summary row
        assert!(
            r.rows
                .iter()
                .any(|row| row[0] == gleaph_types::Value::Text("summary".into()))
        );
    }

    #[test]
    fn explain_gql_returns_plan_lines() {
        state::init_state(8, 0).expect("init state");
        let r = explain_gql("MATCH (u:User) RETURN COUNT(u)".into()).unwrap();
        assert_eq!(r.columns, vec!["line"]);
        assert!(r.rows.iter().any(|row| {
            row[0] == gleaph_types::Value::Text("ops=NodeScan,Aggregate,Project".into())
        }));
        assert!(r.rows.iter().any(|row| {
            matches!(
                &row[0],
                gleaph_types::Value::Text(line) if line == "semantic-aggregates=count"
            )
        }));
    }

    #[test]
    fn explain_gql_includes_impossible_pattern_warning_lines() {
        state::init_state(8, 0).expect("init state");
        mutate_gql(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:WORKS_AT]->, (:Person)-[:WORKS_AT]->(:Company) }"
                .into(),
            None,
        )
        .expect("create graph type");
        let r = explain_gql("MATCH (a:Company)-[:WORKS_AT]->(b:Company) RETURN a, b".into())
            .expect("explain");
        assert_eq!(r.columns, vec!["line"]);
        assert!(r.rows.iter().any(|row| {
            matches!(
                &row[0],
                gleaph_types::Value::Text(line)
                    if line.starts_with("type-warning=ImpossiblePattern:")
            )
        }));
        assert!(r.rows.iter().any(|row| {
            row[0] == gleaph_types::Value::Text("semantic-impossible-pattern=true".into())
        }));
        assert!(r.rows.iter().any(|row| {
            row[0] == gleaph_types::Value::Text("semantic-impossible-pattern-count=1".into())
        }));
        assert!(
            r.warnings
                .iter()
                .any(|warning| { warning.message.contains("pattern endpoint contradiction") })
        );
    }

    #[test]
    fn query_gql_fast_rejects_impossible_pattern_to_empty_result() {
        state::init_state(8, 0).expect("init state");
        mutate_gql(
            "CREATE GRAPH TYPE Social { (:Person), (:Company), -[:WORKS_AT]->, (:Person)-[:WORKS_AT]->(:Company) }"
                .into(),
            None,
        )
        .expect("create graph type");
        let res = query_gql(
            "MATCH (a:Company)-[:WORKS_AT]->(b:Company) RETURN a, b".into(),
            None,
        )
        .expect("query");
        let r = res.result;
        assert_eq!(r.columns, vec!["a", "b"]);
        assert!(r.rows.is_empty(), "expected empty result, got {:?}", r.rows);
        assert!(
            r.warnings
                .iter()
                .any(|warning| { warning.message.contains("pattern endpoint contradiction") })
        );
    }

    #[test]
    fn show_metrics_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        let res = query_gql("SHOW METRICS".into(), None).unwrap();
        let r = res.result;
        assert_eq!(
            r.columns,
            vec![
                "query_count",
                "mutation_count",
                "rejected_count",
                "algorithm_calls",
                "stable_memory_bytes"
            ]
        );
        assert_eq!(r.rows.len(), 1);
    }

    #[test]
    fn show_aliases_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        let res = query_gql("SHOW ALIASES".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["name", "canister_id"]);
    }

    #[test]
    fn show_graph_types_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        let res = query_gql("SHOW GRAPH TYPES".into(), None).unwrap();
        let r = res.result;
        assert_eq!(
            r.columns,
            vec!["name", "node_label_count", "edge_label_count"]
        );
    }

    #[test]
    fn show_grants_via_query_gql() {
        state::init_state(8, 0).expect("init state");
        let res = query_gql("SHOW GRANTS".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["principal", "level"]);
    }

    #[test]
    fn create_index_via_mutate_gql() {
        state::init_state(8, 0).expect("init state");
        mutate_gql("INSERT (:User {name: 'Alice'})".into(), None).unwrap();
        let res = mutate_gql("CREATE INDEX ON :User(name)".into(), None).unwrap();
        assert!(
            res.result
                .warnings
                .iter()
                .any(|w| w.message.contains("created"))
        );
        // Verify the index is visible via SHOW INDEXES
        let show = query_gql("SHOW INDEXES".into(), None).unwrap();
        assert_eq!(show.result.rows.len(), 1);
        assert_eq!(
            show.result.rows[0][1],
            gleaph_types::Value::Text("name".into())
        );
    }

    #[test]
    fn drop_index_via_mutate_gql() {
        state::init_state(8, 0).expect("init state");
        create_index(EntityType::Vertex, "name".into(), IndexType::Equality).unwrap();
        let res = mutate_gql("DROP INDEX ON :User(name)".into(), None).unwrap();
        assert!(
            res.result
                .warnings
                .iter()
                .any(|w| w.message.contains("dropped"))
        );
        // Verify the index is gone
        let show = query_gql("SHOW INDEXES".into(), None).unwrap();
        assert_eq!(show.result.rows.len(), 0);
    }

    #[test]
    fn drop_index_nonexistent_returns_error() {
        state::init_state(8, 0).expect("init state");
        let err = mutate_gql("DROP INDEX ON :User(name)".into(), None).unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn create_edge_index_via_mutate_gql() {
        state::init_state(8, 0).expect("init state");
        let res = mutate_gql("CREATE INDEX ON -[:KNOWS](since)".into(), None).unwrap();
        assert!(
            res.result
                .warnings
                .iter()
                .any(|w| w.message.contains("created"))
        );
        let show = query_gql("SHOW INDEXES".into(), None).unwrap();
        assert!(show.result.rows.iter().any(|row| {
            row[0] == gleaph_types::Value::Text("Edge".into())
                && row[1] == gleaph_types::Value::Text("since".into())
        }));
    }

    #[test]
    fn grant_revoke_via_mutate_gql() {
        state::init_state(8, 0).expect("init state");
        let pid = "aaaaa-aa";
        let res = mutate_gql(format!("GRANT WRITE ON GRAPH TO '{pid}'"), None).unwrap();
        assert!(
            res.result
                .warnings
                .iter()
                .any(|w| w.message.contains("granted"))
        );
        // Verify via SHOW GRANTS
        let show = query_gql("SHOW GRANTS".into(), None).unwrap();
        assert!(show.result.rows.iter().any(|row| {
            row[0] == gleaph_types::Value::Text(pid.into())
                && row[1] == gleaph_types::Value::Text("Write".into())
        }));
        // Revoke
        let res = mutate_gql(format!("REVOKE ACCESS ON GRAPH FROM '{pid}'"), None).unwrap();
        assert!(
            res.result
                .warnings
                .iter()
                .any(|w| w.message.contains("revoked"))
        );
        let show = query_gql("SHOW GRANTS".into(), None).unwrap();
        assert!(show.result.rows.is_empty());
    }

    #[test]
    fn grant_invalid_principal_returns_error() {
        state::init_state(8, 0).expect("init state");
        let err = mutate_gql("GRANT READ ON GRAPH TO 'not-a-principal'".into(), None).unwrap_err();
        assert!(err.to_string().contains("invalid principal"));
    }

    #[test]
    fn analyze_via_mutate_gql() {
        state::init_state(8, 0).expect("init state");
        let res = mutate_gql("ANALYZE".into(), None).unwrap();
        assert!(
            res.result
                .warnings
                .iter()
                .any(|w| w.message.contains("refreshed"))
        );
    }

    #[test]
    fn show_statement_rejected_in_mutation_context() {
        state::init_state(8, 0).expect("init state");
        let err = mutate_gql("SHOW STATS".into(), None).unwrap_err();
        assert!(err.to_string().contains("read-only"));
    }

    #[test]
    fn create_index_rejected_in_query_context() {
        state::init_state(8, 0).expect("init state");
        let err = query_gql("CREATE INDEX ON :User(name)".into(), None).unwrap_err();
        assert!(err.to_string().contains("mutation endpoint"));
    }

    #[test]
    fn analyze_rejected_in_query_context() {
        state::init_state(8, 0).expect("init state");
        let err = query_gql("ANALYZE".into(), None).unwrap_err();
        assert!(err.to_string().contains("mutation endpoint"));
    }

    #[test]
    fn grant_rejected_in_query_context() {
        state::init_state(8, 0).expect("init state");
        let err = query_gql("GRANT WRITE ON GRAPH TO 'aaaaa-aa'".into(), None).unwrap_err();
        assert!(err.to_string().contains("mutation endpoint"));
    }

    // ── CALL procedure tests ──────────────────────────────────────────────────

    #[test]
    fn call_bfs_via_query_gql() {
        state::init_state(16, 0).expect("init state");
        mutate_gql("INSERT (:Node {id: 0})".into(), None).unwrap();
        mutate_gql("INSERT (:Node {id: 1})".into(), None).unwrap();
        mutate_gql("INSERT (:Node {id: 2})".into(), None).unwrap();
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("LINK".into()), Vec::new(), 1.0, 0)
                .unwrap();
            g.create_edge(1, 2, Some("LINK".into()), Vec::new(), 1.0, 0)
                .unwrap();
        });

        let res = query_gql(
            "CALL bfs(0, {max_depth: 5}) YIELD vertex_id, distance".into(),
            None,
        )
        .unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["vertex_id", "distance"]);
        assert!(!r.rows.is_empty(), "BFS should return results");
    }

    #[test]
    fn call_pagerank_via_query_gql() {
        state::init_state(16, 0).expect("init state");
        mutate_gql("INSERT (:Node {id: 0})".into(), None).unwrap();
        mutate_gql("INSERT (:Node {id: 1})".into(), None).unwrap();
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("LINK".into()), Vec::new(), 1.0, 0)
                .unwrap();
        });

        let res = query_gql(
            "CALL pagerank({damping: 0.85, max_iterations: 10}) YIELD vertex_id, score".into(),
            None,
        )
        .unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["vertex_id", "score"]);
        assert!(!r.rows.is_empty(), "PageRank should return results");
    }

    #[test]
    fn call_sssp_via_query_gql() {
        state::init_state(16, 0).expect("init state");
        mutate_gql("INSERT (:Node {id: 0})".into(), None).unwrap();
        mutate_gql("INSERT (:Node {id: 1})".into(), None).unwrap();
        state::with_state_mut(|g| {
            g.create_edge(0, 1, Some("LINK".into()), Vec::new(), 1.0, 0)
                .unwrap();
        });

        let res = query_gql("CALL sssp(0, {}) YIELD vertex_id, distance".into(), None).unwrap();
        let r = res.result;
        assert_eq!(r.columns, vec!["vertex_id", "distance"]);
        assert!(!r.rows.is_empty(), "SSSP should return results");
    }

    #[test]
    fn call_unknown_procedure_returns_error() {
        state::init_state(8, 0).expect("init state");
        let err = query_gql("CALL unknown_proc(1) YIELD x".into(), None).unwrap_err();
        assert!(err.to_string().contains("unknown procedure"));
    }

    #[test]
    fn call_bfs_invalid_yield_column_returns_error() {
        state::init_state(8, 0).expect("init state");
        let err = query_gql("CALL bfs(0) YIELD bogus_column".into(), None).unwrap_err();
        assert!(err.to_string().contains("does not yield column"));
    }
}
