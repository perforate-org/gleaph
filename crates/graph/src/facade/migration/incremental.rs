//! Incremental vertex migration: chunked copy, journal, cutover.

use super::super::store::{EdgeHandle, GraphStore, GraphStoreError};
use super::index::{
    exported_vertex_for_index_sync, remove_source_index_postings_for_vertex,
    sync_migration_index_postings,
};
use super::prune_source::{
    enqueue_prune_migrated_source, prune_queue_has_item, stub_has_live_edge_payload,
    stub_has_vertex_payload,
};
use super::vertex::{export_out_edge, import_out_edge};
use crate::facade::stable::{
    MIGRATION_JOURNAL, MIGRATION_OUT_HANDLE_MAP, MIGRATION_QUEUE, MIGRATION_REV_HANDLE_MAP,
    VERTEX_MIGRATION_STATE,
};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;
use crate::plan::PlanQueryError;
use gleaph_gql::Value;
use gleaph_gql_ic::IcExtensionBinaryDecode;
use gleaph_graph_kernel::entry::{
    Edge, EdgeLabelId, EdgeTarget, EdgeValuePayload, Vertex, VertexRef,
};
use gleaph_graph_kernel::federation::{
    BeginVertexMigrationArgs, ExportedInReverseEdge, ExportedOutEdge, ExportedProperty,
    FinishVertexMigrationArgs, LocalVertexId, LogicalVertexId, MigrationApplyChunk,
    MigrationEdgeHandleWire, MigrationItem, MigrationJournalEntry, MigrationJournalOp,
    MigrationMetadataSnapshot, MigrationOrientation, MigrationPhase, MigrationReconcileAction,
    MigrationReconcileReport, MigrationStagingArgs, MigrationStartResult, MigrationStatus,
    PhysicalVertexLocation, RouterError, ShardId, VertexMigrationState, VertexPlacement,
};
use ic_stable_lara::traits::CsrEdgeTombstone;
use ic_stable_lara::{BucketLabelKey as LaraLabelId, VertexId};
use std::cell::RefCell;

thread_local! {
    static NATIVE_PENDING_APPLY: RefCell<Option<MigrationApplyChunk>> = const { RefCell::new(None) };
}

fn local_raw(vertex_id: VertexId) -> LocalVertexId {
    placement::local_vertex_id_raw(vertex_id)
}

pub(crate) fn migration_wire_handle(
    owner: VertexId,
    label_id: LaraLabelId,
    slot_index: u32,
) -> MigrationEdgeHandleWire {
    MigrationEdgeHandleWire {
        owner_local_vertex_id: local_raw(owner),
        label_raw: u32::from(label_id.raw()),
        slot_index,
    }
}

fn handle_from_wire(owner: VertexId, wire: MigrationEdgeHandleWire) -> EdgeHandle {
    EdgeHandle::at_slot(
        owner,
        LaraLabelId::from_raw(wire.label_raw as u16),
        wire.slot_index,
    )
}

fn wire_handle(owner: VertexId, label_id: LaraLabelId, slot_index: u32) -> MigrationEdgeHandleWire {
    migration_wire_handle(owner, label_id, slot_index)
}

pub fn vertex_migration_state(vertex_id: VertexId) -> VertexMigrationState {
    VERTEX_MIGRATION_STATE
        .with_borrow(|m| m.get(local_raw(vertex_id)))
        .unwrap_or(VertexMigrationState::Active)
}

pub fn vertex_visible_to_query(vertex_id: VertexId) -> bool {
    !matches!(
        vertex_migration_state(vertex_id),
        VertexMigrationState::TargetStaging { .. } | VertexMigrationState::ForwardingStub { .. }
    )
}

pub fn migration_visibility_filter_needed() -> bool {
    VERTEX_MIGRATION_STATE.with_borrow(|m| !m.is_empty())
}

/// Source-shard stub row for `logical_vertex_id` when router placement is authoritative elsewhere.
pub(crate) async fn forwarding_stub_on_current_shard(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Option<LocalVertexId> {
    let routing = store.federation_routing()?;
    let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)
        .await
        .ok()?;
    let VertexPlacement::Active(authoritative) = placement else {
        return None;
    };
    if routing.shard_id == authoritative.shard_id {
        return None;
    }
    let mut stub_local = None;
    VERTEX_MIGRATION_STATE.with_borrow(|m| {
        m.for_each(|local, state| {
            if let VertexMigrationState::ForwardingStub {
                logical_vertex_id: lid,
                cached_location,
                ..
            } = state
            {
                if lid == logical_vertex_id
                    && cached_location.shard_id == authoritative.shard_id
                    && stub_local.is_none()
                {
                    stub_local = Some(local);
                }
            }
        });
    });
    stub_local
}

fn set_migration_state(vertex_id: VertexId, state: VertexMigrationState) {
    VERTEX_MIGRATION_STATE.with_borrow_mut(|m| {
        let local = local_raw(vertex_id);
        if state == VertexMigrationState::Active {
            m.remove(local);
        } else {
            m.insert(local, state);
        }
    });
}

async fn resolve_migrating_epoch(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Result<u64, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;
    let placement =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).await?;
    match placement {
        VertexPlacement::Migrating { epoch, .. } => Ok(epoch),
        _ => Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        )),
    }
}

fn load_item(logical: LogicalVertexId) -> Option<MigrationItem> {
    MIGRATION_QUEUE.with_borrow(|q| q.get(logical))
}

fn save_item(item: MigrationItem) {
    MIGRATION_QUEUE.with_borrow_mut(|q| q.insert(item.logical_vertex_id, item));
}

fn remove_item(logical: LogicalVertexId) {
    MIGRATION_QUEUE.with_borrow_mut(|q| q.remove(logical));
}

fn append_journal(entry: MigrationJournalEntry) {
    MIGRATION_JOURNAL.with_borrow_mut(|j| j.append(entry));
}

fn next_journal_seq(logical: LogicalVertexId, epoch: u64) -> u64 {
    MIGRATION_JOURNAL.with_borrow(|j| j.count_for(logical, epoch))
}

fn capture_metadata_snapshot(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<MigrationMetadataSnapshot, GraphStoreError> {
    let vertex = store
        .vertex(vertex_id)
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;
    let labels = store.vertex_labels(vertex_id, vertex);
    let properties = store
        .vertex_properties(vertex_id)
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                value_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property encode: {e}"),
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;
    Ok(MigrationMetadataSnapshot { labels, properties })
}

fn adjust_bulk_limit(item: &mut MigrationItem, work_units: usize) {
    if work_units as u32 >= item.bulk_limit {
        item.bulk_limit = (item.bulk_limit.saturating_mul(2)).min(MigrationItem::MAX_BULK_LIMIT);
    } else if work_units > 0 && work_units * 2 < item.bulk_limit as usize {
        item.bulk_limit = (item.bulk_limit / 2).max(MigrationItem::MIN_BULK_LIMIT);
    }
}

/// Source shard: begin router migration and mark source vertex migrating.
pub async fn migration_start(
    store: &GraphStore,
    args: BeginVertexMigrationArgs,
) -> Result<MigrationStartResult, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement =
        placement::resolve_placement(routing.router_canister, args.logical_vertex_id).await?;
    let VertexPlacement::Active(source) = placement else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexMigrating),
        ));
    };
    if source.shard_id != routing.shard_id {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::Forbidden),
        ));
    }
    let source_vertex_id = VertexId::from(source.local_vertex_id);

    placement::begin_vertex_migration(routing.router_canister, args)
        .await
        .map_err(GraphStoreError::VertexPlacement)?;

    let epoch = resolve_migrating_epoch(store, args.logical_vertex_id).await?;

    set_migration_state(
        source_vertex_id,
        VertexMigrationState::SourceMigrating { epoch },
    );

    let source_local_id = local_raw(source_vertex_id);
    let item = MigrationItem::new(
        args.logical_vertex_id,
        epoch,
        routing.shard_id,
        source_local_id,
        args.destination_shard_id,
    );
    let metadata_snapshot = capture_metadata_snapshot(store, source_vertex_id)?;
    save_item(item.clone());

    Ok(MigrationStartResult {
        logical_vertex_id: args.logical_vertex_id,
        epoch,
        local_vertex_id: source_local_id,
        metadata_snapshot,
    })
}

/// Destination shard: create staging vertex and copy metadata snapshot.
pub async fn migration_staging_begin(
    store: &GraphStore,
    args: MigrationStagingArgs,
) -> Result<MigrationStartResult, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement =
        placement::resolve_placement(routing.router_canister, args.logical_vertex_id).await?;
    let VertexPlacement::Migrating {
        epoch,
        destination_shard_id,
        ..
    } = placement
    else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        ));
    };
    if epoch != args.epoch {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "epoch mismatch".into(),
            )),
        ));
    }
    if destination_shard_id != routing.shard_id {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::Forbidden),
        ));
    }

    let vertex = Vertex::default();
    let vertex_id = store
        .push_migrated_vertex_row(vertex)
        .map_err(GraphStoreError::from)?;
    store.register_logical_vertex_mapping(vertex_id, args.logical_vertex_id);

    set_migration_state(vertex_id, VertexMigrationState::TargetStaging { epoch });

    apply_vertex_metadata_snapshot(store, vertex_id, &args.metadata_snapshot)?;

    let target_local = local_raw(vertex_id);
    if let Some(mut item) = load_item(args.logical_vertex_id) {
        if item.epoch == epoch {
            item.target_local_vertex_id = target_local;
            item.phase = MigrationPhase::OutEdges;
            save_item(item);
        }
    } else {
        let mut item = MigrationItem::new(
            args.logical_vertex_id,
            epoch,
            args.source_shard_id,
            args.source_local_vertex_id,
            routing.shard_id,
        );
        item.target_local_vertex_id = target_local;
        item.phase = MigrationPhase::OutEdges;
        save_item(item);
    }

    Ok(MigrationStartResult {
        logical_vertex_id: args.logical_vertex_id,
        epoch,
        local_vertex_id: target_local,
        metadata_snapshot: args.metadata_snapshot,
    })
}

fn apply_vertex_metadata_snapshot(
    store: &GraphStore,
    dest_vertex_id: VertexId,
    snapshot: &MigrationMetadataSnapshot,
) -> Result<(), GraphStoreError> {
    let vertex_row = store.vertex(dest_vertex_id).expect("staging vertex");
    let vertex_row = store
        .set_vertex_labels(dest_vertex_id, vertex_row, snapshot.labels.clone())
        .map_err(GraphStoreError::from)?;
    store.set_vertex(dest_vertex_id, vertex_row)?;

    for prop in &snapshot.properties {
        let value = Value::from_binary_bytes_with_extensions(
            &prop.value_bytes,
            &IcExtensionBinaryDecode::INSTANCE,
        )
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                "property decode: {e}"
            )))
        })?;
        store
            .set_vertex_property_without_index_pending(dest_vertex_id, prop.property_id, value)
            .map_err(GraphStoreError::from)?;
    }
    Ok(())
}

/// Apply a copy chunk on the destination staging vertex.
pub async fn migration_apply_chunk(
    store: &GraphStore,
    chunk: MigrationApplyChunk,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement = placement::resolve_placement(routing.router_canister, chunk.logical_vertex_id)
        .await
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                "resolve_placement(logical={}): {e}",
                chunk.logical_vertex_id
            )))
        })?;
    let VertexPlacement::Migrating { epoch, .. } = placement else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        ));
    };
    if epoch != chunk.epoch {
        return Ok(());
    }

    let target_id = VertexId::from(chunk.target_local_vertex_id);
    match vertex_migration_state(target_id) {
        VertexMigrationState::TargetStaging { epoch: e } if e == chunk.epoch => {}
        _ => {
            return Err(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                    "target not in TargetStaging".into(),
                )),
            ));
        }
    }

    if chunk.out_edges.len() != chunk.out_edge_source_handles.len() {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidArgument(
                "out_edges and out_edge_source_handles length mismatch".into(),
            )),
        ));
    }

    for (edge, source_handle) in chunk
        .out_edges
        .iter()
        .zip(chunk.out_edge_source_handles.iter())
    {
        import_out_edge(store, target_id, edge)?;
        if let Ok(target_handle) = find_imported_out_edge_handle(store, target_id, edge).await {
            MIGRATION_OUT_HANDLE_MAP.with_borrow_mut(|m| {
                m.insert(
                    chunk.logical_vertex_id,
                    chunk.epoch,
                    *source_handle,
                    migration_wire_handle(
                        target_id,
                        target_handle.label_id,
                        target_handle.slot_index,
                    ),
                );
            });
        }
    }

    for rev in &chunk.in_reverse_edges {
        import_in_reverse_edge(store, target_id, chunk.logical_vertex_id, chunk.epoch, rev)?;
    }

    for entry in &chunk.journal_entries {
        if entry.epoch != chunk.epoch || entry.logical_vertex_id != chunk.logical_vertex_id {
            continue;
        }
        let item = load_item(chunk.logical_vertex_id).unwrap_or_else(|| {
            MigrationItem::new(chunk.logical_vertex_id, chunk.epoch, 0, 0, routing.shard_id)
        });
        apply_journal_to_staging(store, target_id, &item, entry).await?;
    }

    Ok(())
}

async fn local_logical_vertex_on_current_shard(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Option<VertexId> {
    let routing = store.federation_routing()?;
    let placement = placement::resolve_placement(routing.router_canister, logical_vertex_id)
        .await
        .ok()?;
    let VertexPlacement::Active(loc) = placement else {
        return None;
    };
    if loc.shard_id != routing.shard_id {
        return None;
    }
    Some(VertexId::from(loc.local_vertex_id))
}

async fn find_imported_out_edge_handle(
    store: &GraphStore,
    owner_id: VertexId,
    edge: &ExportedOutEdge,
) -> Result<EdgeHandle, GraphStoreError> {
    use super::super::store::helpers::{
        edge_matches_local_neighbor, edge_storage_label, lara_label,
    };
    use gleaph_graph_kernel::entry::EdgeTarget;
    use gleaph_graph_kernel::federation::ExportedEdgeTarget;

    let label = lara_label(edge_storage_label(edge.catalog_label, edge.undirected));
    let value_bytes = edge.value_bytes.as_slice();
    let target = &edge.target;
    let local_neighbor = match target {
        ExportedEdgeTarget::Local { logical_vertex_id } => {
            local_logical_vertex_on_current_shard(store, *logical_vertex_id).await
        }
        ExportedEdgeTarget::Remote { .. } => None,
    };
    let remote_logical = match target {
        ExportedEdgeTarget::Local { logical_vertex_id } => Some(*logical_vertex_id),
        ExportedEdgeTarget::Remote { logical_vertex_id } => Some(*logical_vertex_id),
    };
    store
        .find_first_forward_handle_descending(owner_id, label, |e| {
            if let Some(neighbor) = local_neighbor {
                return edge_matches_local_neighbor(e, neighbor, value_bytes);
            }
            if let Some(logical_vertex_id) = remote_logical {
                if let Some(EdgeTarget::Remote(remote_ref)) = e.edge_target() {
                    return store.logical_vertex_for_remote_ref(remote_ref)
                        == Some(logical_vertex_id)
                        && e.value_bytes() == value_bytes;
                }
                return e.edge_target().is_some_and(|t| match t {
                    EdgeTarget::Remote(r) => {
                        store.logical_vertex_for_remote_ref(r) == Some(logical_vertex_id)
                            && e.value_bytes() == value_bytes
                    }
                    _ => false,
                });
            }
            false
        })?
        .ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: owner_id,
            label_id: label,
            slot_index: u32::MAX,
        })
}

fn import_in_reverse_edge(
    store: &GraphStore,
    target_vertex_id: VertexId,
    logical_vertex_id: LogicalVertexId,
    epoch: u64,
    rev: &ExportedInReverseEdge,
) -> Result<(), GraphStoreError> {
    let pred_ref = if rev.predecessor_is_remote {
        let remote = store.ensure_remote_ref(rev.predecessor_logical_vertex_id);
        VertexRef::remote_ref(remote)
    } else {
        let pred_local = crate::facade::stable::VERTEX_LOGICAL_IDS
            .with_borrow(|m| m.find_vertex_id(rev.predecessor_logical_vertex_id))
            .ok_or(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
            ))?;
        VertexRef::local(pred_local)
    };

    let label = super::super::store::helpers::lara_label(
        super::super::store::helpers::edge_storage_label(rev.catalog_label, false),
    );
    let reverse = Edge {
        target: pred_ref,
        edge_slot_index: gleaph_graph_kernel::entry::EdgeSlotIndex::from_raw(0),
        label_id: 0,
        value: EdgeValuePayload::from_slice(&rev.value_bytes),
    };

    store
        .with_graph_mut(|graph| {
            graph
                .reverse()
                .insert_edge(target_vertex_id, label, reverse)
        })
        .map_err(|e| {
            GraphStoreError::Graph(ic_stable_lara::DeferredBidirectionalLabeledError::Reverse(
                e,
            ))
        })?;

    let handle = store
        .find_first_reverse_handle_descending(target_vertex_id, label, |edge| {
            edge.target == pred_ref && edge.value_bytes() == rev.value_bytes.as_slice()
        })?
        .ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: target_vertex_id,
            label_id: label,
            slot_index: u32::MAX,
        })?;

    MIGRATION_REV_HANDLE_MAP.with_borrow_mut(|m| {
        m.insert(
            logical_vertex_id,
            epoch,
            rev.source_reverse_handle,
            wire_handle(target_vertex_id, label, handle.slot_index),
        );
    });

    for prop in &rev.properties {
        let value = Value::from_binary_bytes_with_extensions(
            &prop.value_bytes,
            &IcExtensionBinaryDecode::INSTANCE,
        )
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                "property decode: {e}"
            )))
        })?;
        store
            .set_edge_property(handle, prop.property_id, value)
            .map_err(GraphStoreError::from)?;
    }
    Ok(())
}

fn remove_imported_in_reverse_edge(
    store: &GraphStore,
    handle: EdgeHandle,
) -> Result<(), GraphStoreError> {
    store.clear_stub_local_edge_sidecars(handle);
    let _ = store.with_graph_mut(|graph| {
        graph
            .remove_reverse_edge_at_slot(handle.owner_vertex_id, handle.label_id, handle.slot_index)
            .map_err(GraphStoreError::from)
    })?;
    Ok(())
}

/// Run one migration maintenance step on this shard; may return a chunk for the destination.
pub async fn migration_maintenance_step(
    store: &GraphStore,
) -> Result<Option<MigrationApplyChunk>, GraphStoreError> {
    let Some((logical, _)) = MIGRATION_QUEUE.with_borrow(|q| q.first_item()) else {
        return Ok(None);
    };
    migration_maintenance_step_for(store, logical).await
}

/// Like [`migration_maintenance_step`] but scoped to one logical vertex (tests / explicit drivers).
pub async fn migration_maintenance_step_for(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Result<Option<MigrationApplyChunk>, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let Some(mut item) = load_item(logical_vertex_id) else {
        return Ok(None);
    };

    let router_epoch = match resolve_migrating_epoch(store, logical_vertex_id).await {
        Ok(epoch) => epoch,
        Err(_) => return Ok(None),
    };
    if item.epoch != router_epoch {
        cleanup_migration_artifacts(logical_vertex_id, item.epoch);
        clear_migration_vertex_states_for_logical(store, logical_vertex_id, item.epoch);
        return Ok(None);
    }

    if routing.shard_id == item.source_shard_id {
        return run_source_maintenance_step(store, &mut item);
    }
    Ok(None)
}

fn run_source_maintenance_step(
    store: &GraphStore,
    item: &mut MigrationItem,
) -> Result<Option<MigrationApplyChunk>, GraphStoreError> {
    let _source_id = VertexId::from(item.source_local_vertex_id);
    match item.phase {
        MigrationPhase::VertexMetadata => {
            item.phase = MigrationPhase::OutEdges;
            save_item(item.clone());
            Ok(None)
        }
        MigrationPhase::OutEdges => {
            let phase_limit = item.bulk_limit as usize;
            let chunk = copy_out_edges_chunk(store, item)?;
            let copied = chunk.out_edges.len();
            adjust_bulk_limit(item, copied);
            if copied < phase_limit {
                item.phase = MigrationPhase::InReverse;
                item.orientation = MigrationOrientation::InReverse;
                item.label_cursor = Default::default();
                item.edge_cursor = Default::default();
            }
            save_item(item.clone());
            Ok((copied > 0).then_some(chunk))
        }
        MigrationPhase::InReverse => {
            let phase_limit = item.bulk_limit as usize;
            let in_reverse_edges = copy_in_reverse_chunk(store, item)?;
            let copied = in_reverse_edges.len();
            adjust_bulk_limit(item, copied);
            let chunk = MigrationApplyChunk {
                logical_vertex_id: item.logical_vertex_id,
                epoch: item.epoch,
                target_local_vertex_id: item.target_local_vertex_id,
                out_edges: vec![],
                out_edge_source_handles: vec![],
                in_reverse_edges,
                journal_entries: vec![],
            };
            if copied < phase_limit {
                item.phase = MigrationPhase::JournalDrain;
                item.final_seq = Some(next_journal_seq(item.logical_vertex_id, item.epoch));
                item.drained_until_seq = 0;
            }
            save_item(item.clone());
            Ok((copied > 0).then_some(chunk))
        }
        MigrationPhase::JournalDrain => {
            let final_seq = item
                .final_seq
                .unwrap_or_else(|| next_journal_seq(item.logical_vertex_id, item.epoch));
            item.final_seq = Some(final_seq);
            let entries = MIGRATION_JOURNAL.with_borrow(|j| {
                j.entries_for(
                    item.logical_vertex_id,
                    item.epoch,
                    item.drained_until_seq,
                    final_seq.saturating_sub(1),
                )
            });
            let batch: Vec<_> = entries.into_iter().take(item.bulk_limit as usize).collect();
            if let Some(last) = batch.last() {
                item.drained_until_seq = last.seq + 1;
            } else if item.drained_until_seq < final_seq {
                let pending = MIGRATION_JOURNAL.with_borrow(|j| {
                    j.entries_for(
                        item.logical_vertex_id,
                        item.epoch,
                        item.drained_until_seq,
                        final_seq.saturating_sub(1),
                    )
                });
                if pending.is_empty() {
                    item.drained_until_seq = final_seq;
                }
            }
            adjust_bulk_limit(item, batch.len());
            if item.drained_until_seq >= final_seq {
                item.phase = MigrationPhase::Finalize;
            }
            save_item(item.clone());
            let chunk = (!batch.is_empty()).then(|| MigrationApplyChunk {
                logical_vertex_id: item.logical_vertex_id,
                epoch: item.epoch,
                target_local_vertex_id: item.target_local_vertex_id,
                out_edges: vec![],
                out_edge_source_handles: vec![],
                in_reverse_edges: vec![],
                journal_entries: batch,
            });
            Ok(chunk)
        }
        MigrationPhase::Finalize => {
            item.phase = MigrationPhase::Done;
            save_item(item.clone());
            Ok(None)
        }
        MigrationPhase::Done => Ok(None),
    }
}

async fn apply_journal_to_staging(
    store: &GraphStore,
    target_id: VertexId,
    item: &MigrationItem,
    entry: &MigrationJournalEntry,
) -> Result<(), GraphStoreError> {
    let logical = item.logical_vertex_id;
    let epoch = item.epoch;
    match &entry.op {
        MigrationJournalOp::VertexLabelAdded { label_id } => {
            if let Some(vertex) = store.vertex(target_id) {
                let mut labels = store.vertex_labels(target_id, vertex);
                if !labels.contains(label_id) {
                    labels.push(*label_id);
                }
                let vertex = store
                    .set_vertex_labels(target_id, vertex, labels)
                    .map_err(GraphStoreError::from)?;
                store.set_vertex(target_id, vertex)?;
            }
        }
        MigrationJournalOp::VertexLabelRemoved { label_id } => {
            if let Some(vertex) = store.vertex(target_id) {
                let labels: Vec<_> = store
                    .vertex_labels(target_id, vertex)
                    .into_iter()
                    .filter(|l| l != label_id)
                    .collect();
                let vertex = store
                    .set_vertex_labels(target_id, vertex, labels)
                    .map_err(GraphStoreError::from)?;
                store.set_vertex(target_id, vertex)?;
            }
        }
        MigrationJournalOp::VertexPropertySet {
            property_id,
            value_bytes,
        } => {
            let value = Value::from_binary_bytes_with_extensions(
                value_bytes,
                &IcExtensionBinaryDecode::INSTANCE,
            )
            .map_err(|e| {
                GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                    "property decode: {e}"
                )))
            })?;
            store
                .set_vertex_property_without_index_pending(target_id, *property_id, value)
                .map_err(GraphStoreError::from)?;
        }
        MigrationJournalOp::VertexPropertyRemoved { property_id } => {
            store.remove_vertex_property(target_id, *property_id);
        }
        MigrationJournalOp::OutEdgeAdded {
            catalog_label,
            undirected,
            value_bytes,
            target_logical_vertex_id,
            target_is_remote,
            source_handle,
        } => {
            if MIGRATION_OUT_HANDLE_MAP
                .with_borrow(|m| m.get(logical, epoch, *source_handle))
                .is_some()
            {
                return Ok(());
            }
            let edge = exported_out_from_journal(
                *catalog_label,
                *undirected,
                value_bytes,
                *target_logical_vertex_id,
                *target_is_remote,
            );
            import_out_edge(store, target_id, &edge)?;
            if let Ok(h) = find_imported_out_edge_handle(store, target_id, &edge).await {
                MIGRATION_OUT_HANDLE_MAP.with_borrow_mut(|m| {
                    m.insert(
                        logical,
                        epoch,
                        *source_handle,
                        migration_wire_handle(target_id, h.label_id, h.slot_index),
                    );
                });
            }
        }
        MigrationJournalOp::OutEdgeRemoved { source_handle } => {
            if let Some(target_wire) =
                MIGRATION_OUT_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                let _ = store.delete_edge_by_handle(handle);
            }
        }
        MigrationJournalOp::OutEdgeValueChanged {
            source_handle,
            value_bytes,
        } => {
            if let Some(target_wire) =
                MIGRATION_OUT_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                store.update_edge_value_at_handle(handle, value_bytes)?;
            }
        }
        MigrationJournalOp::OutEdgePropertySet {
            source_handle,
            property_id,
            value_bytes,
        } => {
            if let Some(target_wire) =
                MIGRATION_OUT_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                let value = Value::from_binary_bytes_with_extensions(
                    value_bytes,
                    &IcExtensionBinaryDecode::INSTANCE,
                )
                .map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property decode: {e}"),
                    ))
                })?;
                store
                    .set_edge_property(handle, *property_id, value)
                    .map_err(GraphStoreError::from)?;
            }
        }
        MigrationJournalOp::OutEdgePropertyRemoved {
            source_handle,
            property_id,
        } => {
            if let Some(target_wire) =
                MIGRATION_OUT_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                store.remove_edge_property(handle, *property_id);
            }
        }
        MigrationJournalOp::InReverseAdded {
            source_handle,
            predecessor_logical_vertex_id,
            predecessor_is_remote,
            catalog_label,
            canonical_source_handle,
            value_bytes,
        } => {
            if MIGRATION_REV_HANDLE_MAP
                .with_borrow(|m| m.get(logical, epoch, *source_handle))
                .is_some()
            {
                return Ok(());
            }
            let rev = ExportedInReverseEdge {
                catalog_label: *catalog_label,
                value_bytes: value_bytes.clone(),
                predecessor_logical_vertex_id: *predecessor_logical_vertex_id,
                predecessor_is_remote: *predecessor_is_remote,
                source_reverse_handle: *source_handle,
                canonical_source_handle: *canonical_source_handle,
                properties: vec![],
            };
            import_in_reverse_edge(store, target_id, logical, epoch, &rev)?;
        }
        MigrationJournalOp::InReverseRemoved { source_handle } => {
            if let Some(target_wire) =
                MIGRATION_REV_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                remove_imported_in_reverse_edge(store, handle)?;
            }
        }
        MigrationJournalOp::InReverseValueChanged {
            source_handle,
            value_bytes,
        } => {
            if let Some(target_wire) =
                MIGRATION_REV_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                store.update_edge_value_at_handle(handle, value_bytes)?;
            }
        }
        MigrationJournalOp::InReversePropertySet {
            source_handle,
            property_id,
            value_bytes,
        } => {
            if let Some(target_wire) =
                MIGRATION_REV_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                let value = Value::from_binary_bytes_with_extensions(
                    value_bytes,
                    &IcExtensionBinaryDecode::INSTANCE,
                )
                .map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property decode: {e}"),
                    ))
                })?;
                store
                    .set_edge_property(handle, *property_id, value)
                    .map_err(GraphStoreError::from)?;
            }
        }
        MigrationJournalOp::InReversePropertyRemoved {
            source_handle,
            property_id,
        } => {
            if let Some(target_wire) =
                MIGRATION_REV_HANDLE_MAP.with_borrow(|m| m.get(logical, epoch, *source_handle))
            {
                let handle = handle_from_wire(target_id, target_wire);
                store.remove_edge_property(handle, *property_id);
            }
        }
    }
    Ok(())
}

fn exported_out_from_journal(
    catalog_label: Option<EdgeLabelId>,
    undirected: bool,
    value_bytes: &[u8],
    target_logical_vertex_id: LogicalVertexId,
    target_is_remote: bool,
) -> ExportedOutEdge {
    let target = if target_is_remote {
        gleaph_graph_kernel::federation::ExportedEdgeTarget::Remote {
            logical_vertex_id: target_logical_vertex_id,
        }
    } else {
        gleaph_graph_kernel::federation::ExportedEdgeTarget::Local {
            logical_vertex_id: target_logical_vertex_id,
        }
    };
    ExportedOutEdge {
        catalog_label,
        undirected,
        value_bytes: value_bytes.to_vec(),
        target,
        properties: vec![],
    }
}

fn copy_out_edges_chunk(
    store: &GraphStore,
    item: &mut MigrationItem,
) -> Result<MigrationApplyChunk, GraphStoreError> {
    let limit = item.bulk_limit as usize;
    let mut out_edges = Vec::new();
    let mut out_edge_source_handles = Vec::new();
    let copied = copy_out_edges_for_directedness(
        store,
        item,
        false,
        1,
        limit,
        &mut out_edges,
        &mut out_edge_source_handles,
    )?;
    if copied < limit {
        if item.label_cursor.label_raw < 2 {
            item.label_cursor.label_raw = 2;
            item.edge_cursor = Default::default();
        }
        let _ = copy_out_edges_for_directedness(
            store,
            item,
            true,
            2,
            limit - copied,
            &mut out_edges,
            &mut out_edge_source_handles,
        )?;
    }

    Ok(MigrationApplyChunk {
        logical_vertex_id: item.logical_vertex_id,
        epoch: item.epoch,
        target_local_vertex_id: item.target_local_vertex_id,
        out_edges,
        out_edge_source_handles,
        in_reverse_edges: vec![],
        journal_entries: vec![],
    })
}

fn copy_out_edges_for_directedness(
    store: &GraphStore,
    item: &mut MigrationItem,
    undirected: bool,
    cursor_phase: u32,
    limit: usize,
    out_edges: &mut Vec<ExportedOutEdge>,
    out_edge_source_handles: &mut Vec<MigrationEdgeHandleWire>,
) -> Result<usize, GraphStoreError> {
    if limit == 0 || item.label_cursor.label_raw > cursor_phase {
        return Ok(0);
    }
    if item.label_cursor.label_raw == 0 {
        item.label_cursor.label_raw = cursor_phase;
    }
    if item.label_cursor.label_raw < cursor_phase {
        item.label_cursor.label_raw = cursor_phase;
        item.edge_cursor = Default::default();
    }
    if item.label_cursor.label_raw != cursor_phase {
        return Ok(0);
    }

    let source_id = VertexId::from(item.source_local_vertex_id);
    let mut copied = 0usize;
    let mut past_cursor = item.edge_cursor.label_raw == 0 && item.edge_cursor.slot_index == 0;
    let edges = if undirected {
        store.undirected_edges(source_id)?
    } else {
        store.directed_out_edges(source_id)?
    };

    for edge in edges {
        if edge.is_tombstone_edge() {
            continue;
        }
        let bucket = store
            .find_forward_edge_bucket_label(source_id, &edge)?
            .unwrap_or(if undirected {
                LaraLabelId::UNLABELED_UNDIRECTED
            } else {
                LaraLabelId::UNLABELED_DIRECTED
            });
        let slot = edge.edge_slot_index.raw();
        let bucket_raw = u32::from(bucket.raw());
        if !past_cursor {
            if bucket_raw == item.edge_cursor.label_raw && slot >= item.edge_cursor.slot_index {
                past_cursor = true;
            } else if bucket_raw < item.edge_cursor.label_raw {
                continue;
            } else if bucket_raw == item.edge_cursor.label_raw && slot < item.edge_cursor.slot_index
            {
                continue;
            } else {
                past_cursor = true;
            }
        }
        if !past_cursor {
            continue;
        }
        if copied >= limit {
            break;
        }
        out_edges.push(export_out_edge(store, source_id, &edge)?);
        out_edge_source_handles.push(wire_handle(source_id, bucket, slot));
        item.edge_cursor.label_raw = bucket_raw;
        item.edge_cursor.slot_index = slot + 1;
        copied += 1;
    }
    Ok(copied)
}

fn copy_in_reverse_chunk(
    store: &GraphStore,
    item: &mut MigrationItem,
) -> Result<Vec<ExportedInReverseEdge>, GraphStoreError> {
    let target_row = VertexId::from(item.source_local_vertex_id);
    let limit = item.bulk_limit as usize;
    let mut out = Vec::new();
    let mut visited = 0u32;
    for edge in store
        .directed_in_edges(target_row)?
        .into_iter()
        .skip(item.edge_cursor.slot_index as usize)
    {
        if out.len() >= limit {
            break;
        }
        visited = visited.saturating_add(1);
        if !edge.is_tombstone_edge()
            && let Ok(rev) = export_in_reverse_edge(store, target_row, &edge)
        {
            out.push(rev);
        }
    }
    item.edge_cursor.slot_index = item.edge_cursor.slot_index.saturating_add(visited);
    Ok(out)
}

fn export_in_reverse_edge(
    store: &GraphStore,
    row_vertex_id: VertexId,
    edge: &Edge,
) -> Result<ExportedInReverseEdge, GraphStoreError> {
    use super::super::store::helpers::catalog_edge_label_from_wire;

    let bucket = LaraLabelId::from_raw(edge.label_id);
    let slot = edge.edge_slot_index.raw();
    let (predecessor_logical_vertex_id, predecessor_is_remote) = match edge.edge_target() {
        Some(gleaph_graph_kernel::entry::EdgeTarget::Local(v)) => {
            let logical = store
                .logical_vertex_id(v)
                .ok_or(GraphStoreError::VertexPlacement(
                    placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
                ))?;
            // Neighbors that stay on the source shard are imported as remote on the destination.
            let predecessor_is_remote = v != row_vertex_id;
            (logical, predecessor_is_remote)
        }
        Some(gleaph_graph_kernel::entry::EdgeTarget::Remote(r)) => (
            store
                .logical_vertex_for_remote_ref(r)
                .ok_or(GraphStoreError::VertexPlacement(
                    placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
                ))?,
            true,
        ),
        None => {
            return Err(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::InvalidArgument(
                    "reverse edge without predecessor".into(),
                )),
            ));
        }
    };

    let source_reverse_handle = wire_handle(row_vertex_id, bucket, slot);
    let canonical_source_handle = source_reverse_handle;

    let properties = store
        .edge_properties(EdgeHandle::at_slot(row_vertex_id, bucket, slot))
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                value_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property encode: {e}"),
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;

    let canonical_edge = store
        .find_outgoing_edge_record(EdgeHandle::at_slot(row_vertex_id, bucket, slot))?
        .ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: row_vertex_id,
            label_id: bucket,
            slot_index: slot,
        })?;

    Ok(ExportedInReverseEdge {
        catalog_label: catalog_edge_label_from_wire(bucket),
        value_bytes: canonical_edge.value_bytes().to_vec(),
        predecessor_logical_vertex_id,
        predecessor_is_remote,
        source_reverse_handle,
        canonical_source_handle,
        properties,
    })
}

fn plan_query_to_store(err: PlanQueryError) -> GraphStoreError {
    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(err.to_string()))
}

pub async fn migration_cutover(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Result<(), GraphStoreError> {
    migration_cutover_impl(store, logical_vertex_id, None).await
}

/// Like [`migration_cutover`], with federated property-index maintenance when an index client is wired.
pub async fn migration_cutover_with_index(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    index: &dyn PropertyIndexLookup,
) -> Result<(), GraphStoreError> {
    migration_cutover_impl(store, logical_vertex_id, Some(index)).await
}

async fn migration_cutover_impl(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).await?;
    let VertexPlacement::Migrating {
        epoch,
        source,
        destination_shard_id,
    } = placement
    else {
        if let VertexPlacement::Active(dest) = placement {
            if let Some(item) = load_item(logical_vertex_id)
                && routing.shard_id == item.source_shard_id
            {
                let source_id = VertexId::from(item.source_local_vertex_id);
                if let Some(ix) = index {
                    remove_source_index_postings_for_vertex(
                        ix,
                        store,
                        source_id,
                        item.source_shard_id,
                        item.source_local_vertex_id,
                    )
                    .await
                    .map_err(plan_query_to_store)?;
                }
                set_migration_state(
                    source_id,
                    VertexMigrationState::ForwardingStub {
                        logical_vertex_id,
                        cached_location: dest,
                        epoch: item.epoch,
                    },
                );
                cleanup_migration_artifacts(logical_vertex_id, item.epoch);
                enqueue_prune_migrated_source(
                    store,
                    logical_vertex_id,
                    item.source_local_vertex_id,
                    item.epoch,
                );
            }
        }
        return Ok(());
    };

    let Some(item) = load_item(logical_vertex_id) else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "no migration item".into(),
            )),
        ));
    };
    if item.epoch != epoch {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "epoch mismatch".into(),
            )),
        ));
    }

    let dest_cutover_ready = routing.shard_id == destination_shard_id
        && matches!(
            vertex_migration_state(VertexId::from(item.target_local_vertex_id)),
            VertexMigrationState::TargetStaging { epoch: e } if e == epoch
        );

    if item.phase != MigrationPhase::Done && !dest_cutover_ready {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "migration not finalized".into(),
            )),
        ));
    }

    if routing.shard_id == destination_shard_id {
        let target_id = VertexId::from(item.target_local_vertex_id);
        placement::finish_vertex_migration(
            routing.router_canister,
            FinishVertexMigrationArgs {
                logical_vertex_id,
                destination_local_vertex_id: item.target_local_vertex_id,
            },
        )
        .await
        .map_err(GraphStoreError::VertexPlacement)?;
        set_migration_state(target_id, VertexMigrationState::Active);
        if let Some(ix) = index {
            let bundle = exported_vertex_for_index_sync(store, &item)?;
            sync_migration_index_postings(
                ix,
                &bundle,
                destination_shard_id,
                item.target_local_vertex_id,
            )
            .await
            .map_err(plan_query_to_store)?;
        }
        // Keep queue/journal/maps until source shard cutover installs ForwardingStub.
    } else if routing.shard_id == source.shard_id {
        let source_id = VertexId::from(source.local_vertex_id);
        let dest = PhysicalVertexLocation::new(destination_shard_id, item.target_local_vertex_id);
        if let Some(ix) = index {
            remove_source_index_postings_for_vertex(
                ix,
                store,
                source_id,
                source.shard_id,
                source.local_vertex_id,
            )
            .await
            .map_err(plan_query_to_store)?;
        }
        set_migration_state(
            source_id,
            VertexMigrationState::ForwardingStub {
                logical_vertex_id,
                cached_location: dest,
                epoch,
            },
        );
        cleanup_migration_artifacts(logical_vertex_id, epoch);
        enqueue_prune_migrated_source(store, logical_vertex_id, source.local_vertex_id, epoch);
    }

    Ok(())
}

fn cleanup_migration_artifacts(logical: LogicalVertexId, epoch: u64) {
    remove_item(logical);
    MIGRATION_JOURNAL.with_borrow_mut(|j| j.remove_migration(logical, epoch));
    MIGRATION_OUT_HANDLE_MAP.with_borrow_mut(|m| m.remove_migration(logical, epoch));
    MIGRATION_REV_HANDLE_MAP.with_borrow_mut(|m| m.remove_migration(logical, epoch));
}

fn clear_migration_vertex_states_for_logical(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    epoch: u64,
) {
    let locals: Vec<LocalVertexId> = VERTEX_MIGRATION_STATE.with_borrow(|m| {
        let mut out = Vec::new();
        m.for_each(|local, state| {
            let vertex_id = VertexId::from(local);
            if store.logical_vertex_id(vertex_id) != Some(logical_vertex_id) {
                return;
            }
            let matches_epoch = match state {
                VertexMigrationState::SourceMigrating { epoch: e }
                | VertexMigrationState::TargetStaging { epoch: e } => e == epoch,
                VertexMigrationState::ForwardingStub { epoch: e, .. } => e == epoch,
                VertexMigrationState::Active => false,
            };
            if matches_epoch {
                out.push(local);
            }
        });
        out
    });
    VERTEX_MIGRATION_STATE.with_borrow_mut(|m| {
        for local in locals {
            m.remove(local);
        }
    });
}

fn migration_maps_nonempty(logical: LogicalVertexId, epoch: u64) -> bool {
    MIGRATION_OUT_HANDLE_MAP.with_borrow(|m| m.has_migration(logical, epoch))
        || MIGRATION_REV_HANDLE_MAP.with_borrow(|m| m.has_migration(logical, epoch))
}

fn find_forwarding_stub(
    logical_vertex_id: LogicalVertexId,
) -> Option<(LocalVertexId, VertexMigrationState)> {
    let mut found = None;
    VERTEX_MIGRATION_STATE.with_borrow(|m| {
        m.for_each(|local, state| {
            if let VertexMigrationState::ForwardingStub {
                logical_vertex_id: lid,
                ..
            } = state
            {
                if lid == logical_vertex_id && found.is_none() {
                    found = Some((local, state));
                }
            }
        });
    });
    found
}

fn find_migration_vertex_locals(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    epoch: u64,
) -> (Option<LocalVertexId>, Option<LocalVertexId>) {
    let mut source = None;
    let mut target = None;
    VERTEX_MIGRATION_STATE.with_borrow(|m| {
        m.for_each(|local, state| {
            let vertex_id = VertexId::from(local);
            if store.logical_vertex_id(vertex_id) != Some(logical_vertex_id) {
                return;
            }
            match state {
                VertexMigrationState::SourceMigrating { epoch: e } if e == epoch => {
                    source = Some(local);
                }
                VertexMigrationState::TargetStaging { epoch: e } if e == epoch => {
                    target = Some(local);
                }
                _ => {}
            }
        });
    });
    (source, target)
}

fn try_rebuild_migration_item(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    epoch: u64,
    source: PhysicalVertexLocation,
    destination_shard_id: ShardId,
) -> Option<MigrationItem> {
    let (source_local, target_local) =
        find_migration_vertex_locals(store, logical_vertex_id, epoch);
    let source_local = source_local?;
    let target_local = target_local?;
    let mut item = MigrationItem::new(
        logical_vertex_id,
        epoch,
        source.shard_id,
        source_local,
        destination_shard_id,
    );
    item.target_local_vertex_id = target_local;
    let journal_len = next_journal_seq(logical_vertex_id, epoch);
    let maps = migration_maps_nonempty(logical_vertex_id, epoch);
    if journal_len > 0 {
        item.phase = MigrationPhase::JournalDrain;
        item.final_seq = Some(journal_len);
        item.drained_until_seq = 0;
    } else if maps {
        item.phase = MigrationPhase::Done;
    } else {
        item.phase = MigrationPhase::OutEdges;
    }
    save_item(item.clone());
    Some(item)
}

/// Reconcile local queue / vertex states with router placement after interruption or epoch drift.
pub async fn migration_reconcile(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Result<MigrationReconcileReport, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let placement =
        placement::resolve_placement(routing.router_canister, logical_vertex_id).await?;

    if let Some(item) = load_item(logical_vertex_id) {
        let router_epoch = match &placement {
            VertexPlacement::Migrating { epoch, .. } => Some(*epoch),
            _ => None,
        };
        if router_epoch != Some(item.epoch) {
            cleanup_migration_artifacts(logical_vertex_id, item.epoch);
            clear_migration_vertex_states_for_logical(store, logical_vertex_id, item.epoch);
            return Ok(MigrationReconcileReport {
                action: MigrationReconcileAction::RemovedStaleEpoch { epoch: item.epoch },
            });
        }
    }

    match placement {
        VertexPlacement::Active(dest) => {
            if let Some(item) = load_item(logical_vertex_id) {
                if routing.shard_id == item.source_shard_id {
                    let source_id = VertexId::from(item.source_local_vertex_id);
                    set_migration_state(
                        source_id,
                        VertexMigrationState::ForwardingStub {
                            logical_vertex_id,
                            cached_location: dest,
                            epoch: item.epoch,
                        },
                    );
                    cleanup_migration_artifacts(logical_vertex_id, item.epoch);
                    enqueue_prune_migrated_source(
                        store,
                        logical_vertex_id,
                        item.source_local_vertex_id,
                        item.epoch,
                    );
                    return Ok(MigrationReconcileReport {
                        action: MigrationReconcileAction::InstalledForwardingStub,
                    });
                }
                cleanup_migration_artifacts(logical_vertex_id, item.epoch);
                return Ok(MigrationReconcileReport {
                    action: MigrationReconcileAction::CleanedOrphanArtifacts { epoch: item.epoch },
                });
            }
            if !prune_queue_has_item(logical_vertex_id) {
                if let Some((stub_local, VertexMigrationState::ForwardingStub { epoch, .. })) =
                    find_forwarding_stub(logical_vertex_id)
                {
                    let stub_id = VertexId::from(stub_local);
                    if stub_has_live_edge_payload(store, stub_id)
                        || stub_has_vertex_payload(store, stub_id)
                    {
                        enqueue_prune_migrated_source(store, logical_vertex_id, stub_local, epoch);
                    }
                }
            }
            Ok(MigrationReconcileReport {
                action: MigrationReconcileAction::NoOp,
            })
        }
        VertexPlacement::Migrating {
            epoch,
            source,
            destination_shard_id,
        } => {
            if load_item(logical_vertex_id).is_some() {
                return Ok(MigrationReconcileReport {
                    action: MigrationReconcileAction::NoOp,
                });
            }
            if try_rebuild_migration_item(
                store,
                logical_vertex_id,
                epoch,
                source,
                destination_shard_id,
            )
            .is_some()
            {
                return Ok(MigrationReconcileReport {
                    action: MigrationReconcileAction::RebuiltQueueItem,
                });
            }
            Ok(MigrationReconcileReport {
                action: MigrationReconcileAction::AwaitingManualIntervention,
            })
        }
    }
}

pub fn migration_status(
    _store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Result<MigrationStatus, GraphStoreError> {
    let item = load_item(logical_vertex_id);
    let journal_len = item
        .as_ref()
        .map(|i| next_journal_seq(logical_vertex_id, i.epoch))
        .unwrap_or(0);
    let local_state = item.as_ref().and_then(|i| {
        VERTEX_MIGRATION_STATE.with_borrow(|m| {
            m.get(i.source_local_vertex_id)
                .or_else(|| m.get(i.target_local_vertex_id))
        })
    });
    let ready_for_cutover = item
        .as_ref()
        .is_some_and(|i| i.phase == MigrationPhase::Done);
    Ok(MigrationStatus {
        item,
        local_state,
        journal_len,
        ready_for_cutover,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MigrationJournalEdgeKind {
    Out,
    InReverse,
}

fn migration_journal_edge_target(
    store: &GraphStore,
    handle: EdgeHandle,
) -> Option<(VertexId, MigrationEdgeHandleWire, MigrationJournalEdgeKind)> {
    let forward = store.canonical_edge_handle(handle);
    if matches!(
        vertex_migration_state(forward.owner_vertex_id),
        VertexMigrationState::SourceMigrating { .. }
    ) && store
        .find_outgoing_edge_record(forward)
        .ok()
        .flatten()
        .is_some()
    {
        return Some((
            forward.owner_vertex_id,
            migration_wire_handle(
                forward.owner_vertex_id,
                forward.label_id,
                forward.slot_index,
            ),
            MigrationJournalEdgeKind::Out,
        ));
    }

    if let Some((edge, _)) = store
        .find_outgoing_edge_with_bucket_label(forward)
        .ok()
        .flatten()
        && let Some(EdgeTarget::Local(target)) = edge.edge_target()
        && matches!(
            vertex_migration_state(target),
            VertexMigrationState::SourceMigrating { .. }
        )
        && let Some((alias_vertex_id, alias_slot_index, reverse_in)) =
            store.alias_for_canonical_edge(forward)
        && reverse_in
        && alias_vertex_id == target
    {
        return Some((
            target,
            migration_wire_handle(target, forward.label_id, alias_slot_index),
            MigrationJournalEdgeKind::InReverse,
        ));
    }

    let reverse = store.canonical_reverse_in_edge_handle(handle);
    if matches!(
        vertex_migration_state(reverse.owner_vertex_id),
        VertexMigrationState::SourceMigrating { .. }
    ) && store
        .find_outgoing_edge_record(reverse)
        .ok()
        .flatten()
        .is_some()
    {
        return Some((
            reverse.owner_vertex_id,
            migration_wire_handle(
                reverse.owner_vertex_id,
                reverse.label_id,
                reverse.slot_index,
            ),
            MigrationJournalEdgeKind::InReverse,
        ));
    }
    None
}

pub(crate) fn journal_vertex_label_added(
    store: &GraphStore,
    vertex_id: VertexId,
    label_id: gleaph_graph_kernel::entry::VertexLabelId,
) -> Result<(), GraphStoreError> {
    maybe_journal_migration_op(
        store,
        vertex_id,
        MigrationJournalOp::VertexLabelAdded { label_id },
    )
}

pub(crate) fn journal_vertex_label_removed(
    store: &GraphStore,
    vertex_id: VertexId,
    label_id: gleaph_graph_kernel::entry::VertexLabelId,
) -> Result<(), GraphStoreError> {
    maybe_journal_migration_op(
        store,
        vertex_id,
        MigrationJournalOp::VertexLabelRemoved { label_id },
    )
}

pub(crate) fn journal_vertex_property_set(
    store: &GraphStore,
    vertex_id: VertexId,
    property_id: gleaph_graph_kernel::entry::PropertyId,
    value: &Value,
) -> Result<(), GraphStoreError> {
    let value_bytes = value.to_binary_bytes().map_err(|e| {
        GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
            "property encode: {e}"
        )))
    })?;
    maybe_journal_migration_op(
        store,
        vertex_id,
        MigrationJournalOp::VertexPropertySet {
            property_id,
            value_bytes,
        },
    )
}

pub(crate) fn journal_vertex_property_removed(
    store: &GraphStore,
    vertex_id: VertexId,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), GraphStoreError> {
    maybe_journal_migration_op(
        store,
        vertex_id,
        MigrationJournalOp::VertexPropertyRemoved { property_id },
    )
}

pub(crate) fn journal_edge_removed(
    store: &GraphStore,
    handle: EdgeHandle,
) -> Result<(), GraphStoreError> {
    let Some((owner, wire, kind)) = migration_journal_edge_target(store, handle) else {
        return Ok(());
    };
    let op = match kind {
        MigrationJournalEdgeKind::Out => MigrationJournalOp::OutEdgeRemoved {
            source_handle: wire,
        },
        MigrationJournalEdgeKind::InReverse => MigrationJournalOp::InReverseRemoved {
            source_handle: wire,
        },
    };
    maybe_journal_migration_op(store, owner, op)
}

/// Records an inline edge-value change on a [`VertexMigrationState::SourceMigrating`] vertex.
pub(crate) fn journal_edge_value_changed(
    store: &GraphStore,
    handle: EdgeHandle,
    value_bytes: &[u8],
) -> Result<(), GraphStoreError> {
    let Some((owner, wire, kind)) = migration_journal_edge_target(store, handle) else {
        return Ok(());
    };
    let op = match kind {
        MigrationJournalEdgeKind::Out => MigrationJournalOp::OutEdgeValueChanged {
            source_handle: wire,
            value_bytes: value_bytes.to_vec(),
        },
        MigrationJournalEdgeKind::InReverse => MigrationJournalOp::InReverseValueChanged {
            source_handle: wire,
            value_bytes: value_bytes.to_vec(),
        },
    };
    maybe_journal_migration_op(store, owner, op)
}

/// Records an edge property set/remove on a source-migrating vertex.
pub(crate) fn journal_edge_property_changed(
    store: &GraphStore,
    handle: EdgeHandle,
    property_id: gleaph_graph_kernel::entry::PropertyId,
    value_bytes: Option<Vec<u8>>,
) -> Result<(), GraphStoreError> {
    let Some((owner, wire, kind)) = migration_journal_edge_target(store, handle) else {
        return Ok(());
    };
    let op = match (kind, value_bytes) {
        (MigrationJournalEdgeKind::Out, Some(bytes)) => MigrationJournalOp::OutEdgePropertySet {
            source_handle: wire,
            property_id,
            value_bytes: bytes,
        },
        (MigrationJournalEdgeKind::Out, None) => MigrationJournalOp::OutEdgePropertyRemoved {
            source_handle: wire,
            property_id,
        },
        (MigrationJournalEdgeKind::InReverse, Some(bytes)) => {
            MigrationJournalOp::InReversePropertySet {
                source_handle: wire,
                property_id,
                value_bytes: bytes,
            }
        }
        (MigrationJournalEdgeKind::InReverse, None) => {
            MigrationJournalOp::InReversePropertyRemoved {
                source_handle: wire,
                property_id,
            }
        }
    };
    maybe_journal_migration_op(store, owner, op)
}

pub(crate) fn maybe_journal_migration_op(
    store: &GraphStore,
    vertex_id: VertexId,
    op: MigrationJournalOp,
) -> Result<(), GraphStoreError> {
    let VertexMigrationState::SourceMigrating { epoch } = vertex_migration_state(vertex_id) else {
        return Ok(());
    };
    let logical = store
        .logical_vertex_id(vertex_id)
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;
    let Some(item) = load_item(logical) else {
        return Ok(());
    };
    if item.epoch != epoch {
        return Ok(());
    }
    let seq = next_journal_seq(logical, epoch);
    append_journal(MigrationJournalEntry {
        logical_vertex_id: logical,
        epoch,
        seq,
        op,
    });
    Ok(())
}

pub fn take_native_pending_apply() -> Option<MigrationApplyChunk> {
    NATIVE_PENDING_APPLY.with_borrow_mut(|p| p.take())
}

pub fn set_native_pending_apply(chunk: MigrationApplyChunk) {
    NATIVE_PENDING_APPLY.with_borrow_mut(|p| *p = Some(chunk));
}


#[cfg(test)]
mod tests;
