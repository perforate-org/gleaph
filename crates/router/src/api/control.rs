//! L2 control-plane surface (ADR 0056 §4).
//!
//! The audience is the CLI and the graph-admin UI: graph lifecycle, RBAC, schema seams, vector
//! semantic management, maintenance, and diagnostics summaries. Internal design (shards, backfill
//! machinery, vector physical design, provisioning) stays hidden behind graph-level concepts.

use candid::Principal;
use ic_cdk::api::msg_caller;
use ic_cdk_macros::{query, update};

use crate::facade::auth;
#[cfg(feature = "pocket-ic-e2e")]
use crate::facade::stable::label_stats::ClientMutationKey;
use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types;

#[query]
fn whoami() -> Principal {
    msg_caller()
}

#[query]
fn my_role() -> Result<String, RouterError> {
    Ok(auth::caller_role(&msg_caller()).to_string())
}

/// Apply one versioned, additive schema migration. Authorization, checksum verification, AST
/// validation, catalog mutation, and durable ledger insertion are owned by the Router store so
/// the public Candid surface cannot bypass those invariants.
#[update]
async fn apply_schema_migration(
    args: gleaph_migration_api::ApplySchemaMigrationArgs,
) -> Result<gleaph_migration_api::ApplySchemaMigrationResult, RouterError> {
    RouterStore::new()
        .admin_apply_schema_migration_control(
            msg_caller(),
            args,
            &crate::facade::store::real_index_migration_driver(),
        )
        .await
}

/// List the bounded global schema-migration chain in canonical parent order.
#[query]
fn list_schema_migrations(
    args: gleaph_migration_api::ListSchemaMigrationsArgs,
) -> Result<gleaph_migration_api::ListSchemaMigrationsResult, RouterError> {
    RouterStore::new().list_schema_migrations(args)
}

#[update]
fn grant_role(args: types::GrantRoleArgs) -> Result<(), RouterError> {
    let role = auth::parse_role(&args.role).map_err(RouterError::InvalidArgument)?;
    auth::admin_upsert_principal(&msg_caller(), args.target, role, args.manager_caps).map_err(|e| {
        if e.contains("required") {
            RouterError::Forbidden
        } else {
            RouterError::InvalidArgument(e)
        }
    })
}

#[query]
fn get_graph(
    graph_name: String,
) -> Result<gleaph_gql_ic::graph_registry::GraphRegistryEntry, RouterError> {
    RouterStore::new().get_graph_operator(&graph_name, msg_caller())
}

/// Registry-local summary rows for every graph visible to the caller (ADR 0056 §7). No
/// cross-canister calls, so UI/CLI polling stays cheap.
#[query]
fn list_graphs() -> Result<Vec<types::GraphSummary>, RouterError> {
    RouterStore::new().list_graph_summaries(msg_caller())
}

/// Best-effort graph-level operational snapshot (ADR 0056 §7): shard reachability, index-sync
/// convergence, and vector-index health summary. Composite query: probes every shard's index-sync
/// status (reachability + convergence) and summarizes vector-index health. Per-shard and physical
/// detail stay at L3; failures are reported in `notes`, not as errors.
#[query(composite = true)]
async fn get_graph_health(graph_name: String) -> Result<types::GraphHealthView, RouterError> {
    RouterStore::new()
        .graph_health_view(
            msg_caller(),
            &graph_name,
            crate::graph_client::index_sync_status,
        )
        .await
}

/// Intent-based graph creation (ADR 0056 §6 Slice A): dev mode registers the graph and its
/// shards synchronously; provisioned mode is `NotImplemented` until Slice B.
#[update]
async fn register_graph(args: types::RegisterGraphArgs) -> Result<(), RouterError> {
    use gleaph_gql_ic::graph_registry::{GraphStatus, ProvisioningState};

    auth::require_admin(&msg_caller())?;
    if crate::provisioning::config::get().is_some() {
        return Err(RouterError::NotImplemented(
            "provisioned graph registration lands in ADR 0056 Slice B".into(),
        ));
    }
    if args.shards.is_empty() {
        return Err(RouterError::InvalidArgument(
            "dev-mode register_graph requires at least one shard".into(),
        ));
    }
    let caller = msg_caller();
    let store = RouterStore::new();
    let entry = gleaph_gql_ic::graph_registry::GraphRegistryEntry {
        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0), // store assigns
        graph_name: args.graph_name.clone(),
        canister_id: args.shards[0].graph_canister,
        owner: args.owner,
        admins: args.admins,
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: ic_cdk::api::time(),
        provisioning_state: ProvisioningState::None,
        is_home: args.is_home,
    };
    store
        .admin_register_graph_with_random_key(caller, entry)
        .await?;
    for shard in args.shards {
        store
            .admin_register_shard(
                caller,
                types::AdminRegisterShardArgs {
                    shard_id: shard.shard_id,
                    graph_canister: shard.graph_canister,
                    index_canister: shard.index_canister,
                    logical_graph_name: args.graph_name.clone(),
                },
            )
            .await?;
    }
    Ok(())
}

#[update]
fn unregister_graph(logical_graph_name: String) -> Result<(), RouterError> {
    RouterStore::new().admin_unregister_graph(msg_caller(), &logical_graph_name)
}

#[update]
fn ensure_vertex_label(
    logical_graph_name: String,
    name: String,
) -> Result<types::VertexLabelId, RouterError> {
    RouterStore::new().admin_intern_vertex_label(msg_caller(), &logical_graph_name, &name)
}

#[update]
fn ensure_edge_label(
    logical_graph_name: String,
    name: String,
) -> Result<types::EdgeLabelId, RouterError> {
    RouterStore::new().admin_intern_edge_label(msg_caller(), &logical_graph_name, &name)
}

#[update]
fn ensure_properties(
    logical_graph_name: String,
    names: Vec<String>,
) -> Result<Vec<types::PropertyId>, RouterError> {
    RouterStore::new().admin_intern_properties(msg_caller(), &logical_graph_name, &names)
}

#[update]
async fn index_vertex_property(
    logical_graph_name: String,
    vertex_label: String,
    property: String,
) -> Result<(), RouterError> {
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    crate::index_catalog::create_admin_compat_property_index(
        graph_id,
        crate::index_ddl::IndexTarget {
            kind: IndexedPropertyKind::Vertex,
            label: vertex_label,
            property,
            edge_direction: None,
        },
    )
    .await
}

#[update]
async fn index_edge_property(
    logical_graph_name: String,
    edge_label: String,
    property: String,
) -> Result<(), RouterError> {
    use gleaph_gql::types::EdgeDirection;
    use gleaph_graph_kernel::index::IndexedPropertyKind;

    crate::rbac::authorize_index_ddl(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    crate::index_catalog::create_admin_compat_property_index(
        graph_id,
        crate::index_ddl::IndexTarget {
            kind: IndexedPropertyKind::Edge,
            label: edge_label,
            property,
            edge_direction: Some(EdgeDirection::AnyDirection),
        },
    )
    .await
}

/// Register a derived vector index (ADR 0031 Slice 3; `authorize_index_ddl`). Returns whether the
/// definition was newly created. Production dispatch stays fail-closed until incarnation fencing.
#[update]
fn admin_register_vector_index(args: types::RegisterVectorIndexArgs) -> Result<bool, RouterError> {
    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorIndexKind, VectorMetric};

    crate::rbac::authorize_index_ddl(&msg_caller())?;
    if args.embedding_name.is_empty() {
        return Err(RouterError::InvalidArgument(
            "embedding_name must not be empty".to_owned(),
        ));
    }
    if args.dims == 0 {
        return Err(RouterError::InvalidArgument("dims must be > 0".to_owned()));
    }
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    let target = args
        .target
        .map(|canister| vector_index_catalog::VectorIndexTarget { canister });
    // Preflight (conflict / if-not-exists no-op / anonymous-target rejection) BEFORE interning the
    // embedding name or resolving labels, so a rejected or no-op registration never allocates a durable
    // EmbeddingNameId (which would pollute the graph-scoped name catalog and could exhaust the u16 name
    // space) and a conflict/no-op reports its own error instead of a label-resolution error.
    if vector_index_catalog::preflight_register(
        graph_id,
        args.index_id,
        target,
        args.if_not_exists,
    )? == vector_index_catalog::RegisterPreflight::AlreadyExists
    {
        return Ok(false);
    }
    // Resolve each label string to a `VertexLabelId` (rejecting an empty set and failing closed on an
    // unknown label) before any durable write, so a rejected registration never allocates an embedding
    // name or a def.
    let labels = vector_index_catalog::resolve_vector_index_labels(&store, graph_id, &args.labels)?;
    let embedding_name_id =
        embedding_name_catalog::intern_embedding_name(graph_id, &args.embedding_name)?;
    // Slice 3 supports exactly one variant of each physical parameter; the wire stays
    // algorithm-neutral and the catalog records the only supported shape.
    vector_index_catalog::register_vector_index(
        graph_id,
        args.index_id,
        embedding_name_id,
        labels,
        VectorIndexKind::IvfFlat,
        args.metric.unwrap_or(VectorMetric::L2Squared),
        args.encoding.unwrap_or(VectorEncoding::F32),
        args.dims,
        target,
        args.if_not_exists,
    )
}

/// List the derived vector-index definitions registered for a logical graph (ADR 0031 Slice 3).
#[query]
fn list_vector_indexes(
    logical_graph_name: String,
) -> Result<Vec<types::VectorIndexInfo>, RouterError> {
    use crate::facade::stable::vector_index_catalog;

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&logical_graph_name)?;
    let dispatch_ready = store.graph_vector_dispatch_ready(graph_id);
    Ok(vector_index_catalog::list_vector_indexes(graph_id)
        .iter()
        .map(|def| vector_index_catalog::vector_index_info(def, dispatch_ready))
        .collect())
}

/// Ingest finite F32 vertex embeddings via the Router-initiated two-call flow (ADR 0064 §6):
/// Router → Graph `stamp_embedding` (validate + consume a `mutation_id`, no byte storage), then
/// Router → Vector (bytes + stamp). The graph holds no embedding bytes; the vector canister is the
/// sole owner. Items are validated up front and grouped by target graph canister.
#[update]
async fn ingest_vertex_embeddings(
    args: types::AdminIngestVertexEmbeddingBatchArgs,
) -> Result<
    Vec<Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>>,
    RouterError,
> {
    crate::rbac::authorize_index_ddl(&msg_caller())?;

    if args.items.is_empty() {
        return Ok(Vec::new());
    }

    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&args.logical_graph_name)?;
    let key = store.graph_element_id_encoding_key(graph_id)?;
    let live_shards = store.list_live_shards_for_graph_id(graph_id)?;

    use crate::facade::stable::{embedding_name_catalog, vector_index_catalog};
    use gleaph_graph_kernel::federation::{EncodedVertexId, decode_global_vertex_id};
    use gleaph_graph_kernel::vector_index::{IndexedEmbeddingSpec, VertexEmbeddingIngestionArgs};

    let name_id = embedding_name_catalog::lookup_embedding_name_id(graph_id, &args.embedding_name)
        .ok_or_else(|| {
            RouterError::NotFound(format!(
                "embedding name {} is not registered for this graph",
                args.embedding_name
            ))
        })?;
    let def = vector_index_catalog::get_vector_index_by_embedding_name_id(graph_id, name_id)
        .ok_or_else(|| {
            RouterError::NotFound(format!(
                "no vector index registered for embedding name {}",
                args.embedding_name
            ))
        })?;
    let vector_canister = def
        .target
        .ok_or_else(|| {
            RouterError::Conflict(format!("vector index {} has no target set", def.index_id))
        })?
        .canister;

    // Model Y: the stored encoding may be F32 or I8; the wire embedding bytes are always canonical
    // F32. The vector canister validates `op.encoding == def.encoding` and quantizes internally.
    if !matches!(
        def.encoding,
        gleaph_graph_kernel::vector_index::VectorEncoding::F32
            | gleaph_graph_kernel::vector_index::VectorEncoding::I8
    ) {
        return Err(RouterError::InvalidArgument(format!(
            "encoding {:?} is not supported for ingestion",
            def.encoding
        )));
    }

    let spec = IndexedEmbeddingSpec {
        embedding_name_id: name_id.raw(),
        index_id: def.index_id,
        kind: def.kind,
        metric: def.metric,
        encoding: def.encoding,
        dims: def.dims,
        labels: def.labels.clone(),
    };

    let item_count = args.items.len();

    // Resolve each item to its target graph canister and group by canister. Each item carries the
    // Router-issued per-shard mutation sequence (ADR 0064 §5/§6) and the shard id needed to build the
    // vector subject.
    type Grouped = std::collections::BTreeMap<
        candid::Principal,
        Vec<(
            VertexEmbeddingIngestionArgs,
            gleaph_graph_kernel::federation::ShardId,
            usize,
        )>,
    >;
    let mut by_canister: Grouped = Grouped::new();
    for (item_index, item) in args.items.into_iter().enumerate() {
        if item.encoded_vertex_id.len() != gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
        {
            return Err(RouterError::InvalidArgument(format!(
                "encoded_vertex_id must be exactly {} bytes",
                gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES
            )));
        }
        if item.values.len() != def.dims as usize {
            return Err(RouterError::InvalidArgument(format!(
                "values length {} does not match vector index dims {}",
                item.values.len(),
                def.dims
            )));
        }
        if item.values.iter().copied().any(|v| !v.is_finite()) {
            return Err(RouterError::InvalidArgument(
                "values must be finite".to_string(),
            ));
        }

        let encoded_bytes: [u8; gleaph_graph_kernel::federation::ENCODED_VERTEX_ID_BYTES] =
            item.encoded_vertex_id.as_slice().try_into().map_err(|_| {
                RouterError::InvalidArgument("encoded_vertex_id conversion failed".to_string())
            })?;
        let global_id = decode_global_vertex_id(&key, EncodedVertexId(encoded_bytes));
        let shard = live_shards
            .iter()
            .find(|s| s.shard_id == global_id.shard_id)
            .ok_or(RouterError::ShardNotRegistered)?;
        // The Router issues the per-shard mutation sequence and passes it to the graph (ADR 0064
        // §5/§6); the graph consumes it as the vector dispatch stamp.
        let mutation_id = store.allocate_mutation_id()?;

        by_canister.entry(shard.graph_canister).or_default().push((
            VertexEmbeddingIngestionArgs {
                local_vertex_id: global_id.local_vertex_id,
                spec: spec.clone(),
                values: item.values,
                mutation_id,
            },
            global_id.shard_id,
            item_index,
        ));
    }

    let mut results: Vec<
        Result<gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult, String>,
    > = Vec::with_capacity(item_count);
    results.resize(item_count, Err("not dispatched".to_string()));

    // Phase 1: Router → Graph `stamp_embedding` (validate + consume the mutation_id, no byte
    // storage). Collect the consumed stamps per item.
    let mut stamped: Vec<(
        gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp,
        usize,
    )> = Vec::new();
    for (graph_canister, mut group) in by_canister {
        group.sort_by_key(|(_, _, original_index)| *original_index);
        for (arg, shard_id, original_index) in group {
            match crate::graph_client::stamp_embedding(graph_canister, arg.clone()).await {
                Ok(stamp) => {
                    let bytes: Vec<u8> = arg.values.iter().flat_map(|v| v.to_le_bytes()).collect();
                    stamped.push((
                        gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp {
                            index_id: spec.index_id,
                            embedding_name_id: spec.embedding_name_id,
                            subject: gleaph_graph_kernel::vector_index::VectorSubject::Vertex {
                                shard_id,
                                vertex_id: arg.local_vertex_id,
                            },
                            mutation_id: stamp,
                            encoding: spec.encoding,
                            dims: spec.dims,
                            metric: spec.metric,
                            bytes,
                            remove: false,
                        },
                        original_index,
                    ));
                }
                Err(err) => {
                    results[original_index] = Err(err);
                }
            }
        }
    }

    // Phase 2: Router → Vector persist (bytes + stamp). The vector canister is the single target for
    // the whole graph, so all stamped ops are delivered in one bounded batch.
    if !stamped.is_empty() {
        let ops: Vec<_> = stamped.iter().map(|(op, _)| op.clone()).collect();
        let progress = crate::vector_sync::vector_sync_batch(vector_canister, ops)
            .await
            .map_err(RouterError::Internal)?;
        let applied = usize::try_from(progress.applied).unwrap_or(0);
        for (index, (op, original_index)) in stamped.into_iter().enumerate() {
            let outcome = if index < applied {
                gleaph_graph_kernel::vector_index::VertexEmbeddingProjectionOutcome::Applied
            } else {
                gleaph_graph_kernel::vector_index::VertexEmbeddingProjectionOutcome::DeferredForRepair
            };
            results[original_index] = Ok(
                gleaph_graph_kernel::vector_index::VertexEmbeddingIngestionResult {
                    embedding_version: op.mutation_id,
                    projection_outcome: outcome,
                },
            );
        }
    }

    Ok(results)
}

/// Advance one bounded unit of graph-index repair work of the given kind across every shard
/// (`Role::Admin`; call in a loop until `all_done`). The Router iterates shards internally.
#[update]
async fn advance_backfill(
    graph_name: String,
    kind: types::BackfillKind,
    max_work: u32,
) -> Result<types::AdvanceBackfillResult, RouterError> {
    use crate::types::{AdvanceBackfillResult, BackfillKind, BackfillShardAdvance};

    if max_work == 0 {
        return Err(RouterError::InvalidArgument("max_work must be > 0".into()));
    }
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let shards = store.list_shards_for_graph_id(graph_id)?;
    let caller = msg_caller();
    let mut shard_results = Vec::with_capacity(shards.len());
    for shard in shards {
        let done =
            match kind {
                BackfillKind::Label => {
                    crate::label_backfill::admin_label_backfill_step(
                        &store,
                        caller,
                        types::AdminLabelBackfillStepArgs {
                            logical_graph_name: graph_name.clone(),
                            shard_id: shard.shard_id,
                            max_vertices: max_work,
                        },
                        crate::graph_client::backfill_label_postings,
                    )
                    .await?
                    .done
                }
                BackfillKind::VertexProperty => {
                    let catalog = crate::facade::stable::indexed_catalog::
                    load_active_indexed_property_catalog(graph_id);
                    crate::vertex_property_backfill::admin_vertex_property_backfill_step(
                        &store,
                        caller,
                        types::AdminVertexPropertyBackfillStepArgs {
                            logical_graph_name: graph_name.clone(),
                            shard_id: shard.shard_id,
                            max_vertices: max_work,
                        },
                        move |graph, bargs| {
                            crate::graph_client::backfill_vertex_property_postings(
                                graph,
                                bargs,
                                catalog.clone(),
                            )
                        },
                    )
                    .await?
                    .done
                }
                BackfillKind::Edge => {
                    let catalog = crate::facade::stable::indexed_catalog::
                    load_active_indexed_property_catalog(graph_id);
                    crate::edge_backfill::admin_edge_backfill_step(
                        &store,
                        caller,
                        types::AdminEdgeBackfillStepArgs {
                            logical_graph_name: graph_name.clone(),
                            shard_id: shard.shard_id,
                            max_entries: max_work,
                        },
                        move |graph, bargs| {
                            crate::graph_client::backfill_edge_property_postings(
                                graph,
                                bargs,
                                catalog.clone(),
                            )
                        },
                    )
                    .await?
                    .done
                }
                BackfillKind::LabelStats => {
                    crate::label_stats_projection::admin_label_stats_projection_step(
                        &store,
                        caller,
                        types::AdminLabelStatsProjectionStepArgs {
                            logical_graph_name: graph_name.clone(),
                            shard_id: shard.shard_id,
                            max_deltas: max_work,
                        },
                        crate::graph_client::list_pending_label_stats_deltas,
                        crate::graph_client::ack_label_stats_deltas_through,
                    )
                    .await?
                    .done
                }
            };
        shard_results.push(BackfillShardAdvance {
            shard_id: shard.shard_id,
            done,
        });
    }
    let all_done = !shard_results.is_empty() && shard_results.iter().all(|s| s.done);
    Ok(AdvanceBackfillResult {
        all_done,
        shards: shard_results,
    })
}

/// Kind-keyed backfill status for every shard of a logical graph (`Role::Admin`).
#[query]
fn list_backfill_status(
    logical_graph_name: String,
) -> Result<Vec<types::BackfillShardStatus>, RouterError> {
    use crate::types::{BackfillKind, BackfillShardStatus};

    let store = RouterStore::new();
    let caller = msg_caller();
    let mut out = Vec::new();
    let label = crate::label_backfill::admin_list_label_backfill_status(
        &store,
        caller,
        &logical_graph_name,
    )?;
    out.extend(label.into_iter().map(|s| BackfillShardStatus {
        kind: BackfillKind::Label,
        shard_id: s.shard_id,
        done: s.done,
    }));
    let vertex = crate::vertex_property_backfill::admin_list_vertex_property_backfill_status(
        &store,
        caller,
        &logical_graph_name,
    )?;
    out.extend(vertex.into_iter().map(|s| BackfillShardStatus {
        kind: BackfillKind::VertexProperty,
        shard_id: s.shard_id,
        done: s.done,
    }));
    let edge =
        crate::edge_backfill::admin_list_edge_backfill_status(&store, caller, &logical_graph_name)?;
    out.extend(edge.into_iter().map(|s| BackfillShardStatus {
        kind: BackfillKind::Edge,
        shard_id: s.shard_id,
        done: s.done,
    }));
    Ok(out)
}

/// Graph-index convergence snapshot for one graph shard (`Role::Admin`). Poll
/// `converged` before dispatching index-dependent waves; the backfill steps repair
/// convergence when it stalls.
#[update]
async fn get_graph_sync_status(
    args: types::AdminIndexSyncStatusArgs,
) -> Result<gleaph_graph_kernel::federation::IndexSyncStatus, RouterError> {
    RouterStore::new()
        .admin_index_sync_status(msg_caller(), args, crate::graph_client::index_sync_status)
        .await
}

/// Admin-only physical stable-memory inventory for every shard in a graph.
#[query(composite = true)]
async fn get_stable_memory_stats(
    graph_name: String,
) -> Result<Vec<types::GraphStableMemoryStats>, RouterError> {
    crate::rbac::authorize_stable_memory_diagnostics(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let shards = store.list_shards_for_graph_id(graph_id)?;
    let mut stats = Vec::with_capacity(shards.len());
    for shard in shards {
        let memory = crate::graph_client::admin_stable_memory_stats(shard.graph_canister)
            .await
            .map_err(RouterError::Internal)?;
        stats.push(types::GraphStableMemoryStats {
            shard_id: shard.shard_id,
            graph_canister: shard.graph_canister,
            memory,
        });
    }
    Ok(stats)
}

/// Debug-only: dump the in-memory batch instruction log. Requires `batch-instr-log` feature.
#[query]
fn admin_take_batch_instr_log(offset: u32, limit: u32) -> Vec<String> {
    crate::instr_log::dump()
        .into_iter()
        .skip(offset as usize)
        .take(limit.clamp(1, 10_000) as usize)
        .collect()
}

#[cfg(feature = "batch-instr-log")]
/// Admin-only proxy: per-shard batch instruction logs from the Graph shard.
#[query(composite = true)]
async fn admin_graph_batch_instr_log(
    graph_name: String,
    offset: u32,
    limit: u32,
) -> Result<Vec<types::GraphBatchInstrLogPage>, RouterError> {
    crate::rbac::authorize_stable_memory_diagnostics(&msg_caller())?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id(&graph_name)?;
    let shards = store.list_shards_for_graph_id(graph_id)?;
    let mut pages = Vec::with_capacity(shards.len());
    for shard in shards {
        let lines =
            crate::graph_client::admin_take_batch_instr_log(shard.graph_canister, offset, limit)
                .await
                .map_err(RouterError::Internal)?;
        pages.push(types::GraphBatchInstrLogPage {
            shard_id: shard.shard_id,
            graph_canister: shard.graph_canister,
            lines,
        });
    }
    Ok(pages)
}

// --- Test-only seams (`pocket-ic-e2e` feature) ---

/// Test-only (`pocket-ic-e2e`): inject a projection-lagging federated saga referencing an
/// already-committed `mutation_id`, then arm the recovery timer. Lets the E2E suite drive the
/// autonomous recovery driver from `ProjectionPending` to `Completed` without a client retry.
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_inject_projection_pending_saga(
    logical_graph_name: String,
    client_mutation_key: String,
    mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    row_count: u64,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    let shards = store.list_live_shards_for_graph_id(graph_id)?;
    let key = ClientMutationKey::new(caller, graph_id, client_mutation_key);
    store.test_insert_projection_pending_record(&key, mutation_id, row_count, &shards)?;
    crate::recovery::arm_if_needed();
    Ok(())
}

/// Test-only (`pocket-ic-e2e`): declare a uniqueness constraint (admin-authorized, declare-on-empty)
/// so the E2E suite can exercise the ADR 0030 write-path lifecycle. Public `CREATE`/`DROP CONSTRAINT`
/// DDL remains `NotImplemented` (CREATE pending the publication decision, DROP pending a dedicated
/// lifecycle slice — ADR 0030 Revisions #14–#15).
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_declare_unique_constraint(
    logical_graph_name: String,
    constraint_name: String,
    label: String,
    property: String,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    store.create_unique_constraint(graph_id, &constraint_name, false, &label, &property)
}

/// Test-only (`pocket-ic-e2e`): arm (or clear, with `0`) a Router write-path fault injection so
/// E2E suites can reproduce canonical commit / Router callback trap boundaries.
/// Admin-authorized. See [`crate::test_fault`].
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_arm_fault(code: u8) -> Result<(), RouterError> {
    auth::require_admin(&msg_caller())?;
    let fault = crate::test_fault::fault_from_code(code)
        .ok_or_else(|| RouterError::InvalidArgument(format!("unknown fault code {code}")))?;
    crate::test_fault::arm(fault);
    Ok(())
}

/// Test-only (`pocket-ic-e2e`): inspect the durable bulk-load Start allocation boundary for one
/// client key. This exposes only the mutation counter, optional bound mutation id, and identity
/// family so an ingress-trap test can prove IC message rollback without adding production API.
#[cfg(feature = "pocket-ic-e2e")]
#[query]
fn test_bulk_load_start_probe(
    logical_graph_name: String,
    client_bulk_key: String,
) -> Result<(u64, Option<u64>, bool), RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    let counter = crate::facade::stable::ROUTER_MUTATION_COUNTER.with_borrow(|value| *value.get());
    let key = ClientMutationKey::new(caller, graph_id, client_bulk_key);
    let record = store.router_mutation_record(&key);
    let mutation_id = record.as_ref().map(|value| value.as_v1().mutation_id);
    let is_bulk_load = record.as_ref().is_some_and(|value| {
        matches!(
            value.as_v1().request_identity,
            crate::facade::stable::label_stats::RouterMutationRequestIdentityV1::BulkLoadJob
        )
    });
    Ok((counter, mutation_id, is_bulk_load))
}

/// Test-only (`pocket-ic-e2e`): expand one publicly completed receipt into the fixed 65-row GC
/// fixture at the actual stable owner, then suppress the autonomous timer before simulated time is
/// advanced. Public commands still create and complete the template job.
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_seed_bulk_load_gc_fixture(
    logical_graph_name: String,
    client_bulk_key: String,
) -> Result<(), RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    store.test_expand_completed_bulk_load_receipts(caller, graph_id, &client_bulk_key)?;
    crate::recovery::test_pause_for_exact_bulk_gc();
    Ok(())
}

/// Test-only (`pocket-ic-e2e`): execute exactly one production-owned bounded receipt-GC step.
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_bulk_load_gc_step(
    logical_graph_name: String,
    client_bulk_key: String,
) -> Result<(u32, u32, bool), RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    crate::recovery::test_pause_for_exact_bulk_gc();
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    let step =
        store.bulk_load_receipt_gc_step(caller, graph_id, &client_bulk_key, ic_cdk::api::time())?;
    Ok((step.scanned, step.removed, step.done))
}

/// Test-only (`pocket-ic-e2e`): observe parent existence, durable receipt-GC cursor, physical child
/// row count, and preserved terminal outcome without advancing GC.
#[cfg(feature = "pocket-ic-e2e")]
#[query]
fn test_bulk_load_gc_probe(
    logical_graph_name: String,
    client_bulk_key: String,
) -> Result<(bool, Option<u32>, u32, Option<String>), RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    store.test_bulk_load_gc_probe(caller, graph_id, &client_bulk_key)
}

/// Test-only (`pocket-ic-e2e`): force a `Reserved` reservation into `Reclaiming` (admin), so the
/// failure-injection suite can prove a same-`ClaimId` retry is fenced during a reclaim proof.
#[cfg(feature = "pocket-ic-e2e")]
#[update]
fn test_force_reclaiming(
    logical_graph_name: String,
    label: String,
    property: String,
    value: String,
) -> Result<bool, RouterError> {
    let caller = msg_caller();
    auth::require_admin(&caller)?;
    let store = RouterStore::new();
    let graph_id = store.resolve_graph_id_authorized(&logical_graph_name, caller)?;
    store.test_force_reclaiming_text(graph_id, &label, &property, &value)
}
