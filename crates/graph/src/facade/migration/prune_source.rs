//! Source-local cleanup of redundant CSR payload on [`VertexMigrationState::ForwardingStub`] rows.

use super::super::store::{GraphStore, GraphStoreError};
use super::incremental::vertex_migration_state;
use crate::facade::stable::PRUNE_MIGRATED_SOURCE_QUEUE;
use gleaph_graph_kernel::federation::{
    LocalVertexId, LogicalVertexId, PruneMigratedSourceItem, PruneMigratedSourcePhase,
    VertexMigrationState,
};
use ic_stable_lara::traits::CsrEdgeTombstone;
use ic_stable_lara::{BucketLabelKey as LaraLabelId, VertexId};

pub(crate) fn prune_queue_has_item(logical: LogicalVertexId) -> bool {
    load_prune_item(logical).is_some()
}

fn load_prune_item(logical: LogicalVertexId) -> Option<PruneMigratedSourceItem> {
    PRUNE_MIGRATED_SOURCE_QUEUE.with_borrow(|q| q.get(logical))
}

fn save_prune_item(item: PruneMigratedSourceItem) {
    PRUNE_MIGRATED_SOURCE_QUEUE.with_borrow_mut(|q| {
        q.insert(item.logical_vertex_id, item);
    });
}

fn remove_prune_item(logical: LogicalVertexId) {
    PRUNE_MIGRATED_SOURCE_QUEUE.with_borrow_mut(|q| q.remove(logical));
}

fn adjust_prune_bulk_limit(item: &mut PruneMigratedSourceItem, work_units: usize) {
    if work_units as u32 >= item.bulk_limit {
        item.bulk_limit =
            (item.bulk_limit.saturating_mul(2)).min(PruneMigratedSourceItem::MAX_BULK_LIMIT);
    } else if work_units > 0 && work_units * 2 < item.bulk_limit as usize {
        item.bulk_limit = (item.bulk_limit / 2).max(PruneMigratedSourceItem::MIN_BULK_LIMIT);
    }
}

fn stub_out_has_live_edges(store: &GraphStore, source_id: VertexId) -> bool {
    let mut offset = 0;
    let has_directed = store
        .skip_then_visit_each_directed_out_edge(source_id, &mut offset, |edge| {
            Ok::<bool, GraphStoreError>(!edge.is_tombstone_edge())
        })
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false);
    if has_directed {
        return true;
    }
    let mut offset = 0;
    store
        .skip_then_visit_each_undirected_edge(source_id, &mut offset, |edge| {
            Ok::<bool, GraphStoreError>(!edge.is_tombstone_edge())
        })
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false)
}

fn stub_in_has_live_edges(store: &GraphStore, source_id: VertexId) -> bool {
    let mut offset = 0;
    store
        .skip_then_visit_each_directed_in_edge(source_id, &mut offset, |edge| {
            Ok::<bool, GraphStoreError>(!edge.is_tombstone_edge())
        })
        .ok()
        .and_then(Result::ok)
        .unwrap_or(false)
}

pub(crate) fn stub_has_live_edge_payload(store: &GraphStore, source_id: VertexId) -> bool {
    stub_out_has_live_edges(store, source_id) || stub_in_has_live_edges(store, source_id)
}

pub(crate) fn stub_has_vertex_payload(store: &GraphStore, source_id: VertexId) -> bool {
    if let Some(vertex) = store.vertex(source_id)
        && !store.vertex_labels(source_id, vertex).is_empty()
    {
        return true;
    }
    !store.vertex_properties(source_id).is_empty()
}

fn forwarding_stub_epoch(source_id: VertexId) -> Option<u64> {
    match vertex_migration_state(source_id) {
        VertexMigrationState::ForwardingStub { epoch, .. } => Some(epoch),
        _ => None,
    }
}

/// Enqueue source-local stub cleanup after cutover when the physical row still carries payload.
pub fn enqueue_prune_migrated_source(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
    source_local_vertex_id: LocalVertexId,
    epoch: u64,
) {
    if load_prune_item(logical_vertex_id).is_some() {
        return;
    }
    let source_id = VertexId::from(source_local_vertex_id);
    if forwarding_stub_epoch(source_id) != Some(epoch) {
        return;
    }
    if !stub_has_live_edge_payload(store, source_id) && !stub_has_vertex_payload(store, source_id) {
        return;
    }
    save_prune_item(PruneMigratedSourceItem::new(
        logical_vertex_id,
        source_local_vertex_id,
        epoch,
    ));
}

fn prune_out_edges_chunk(
    store: &GraphStore,
    item: &mut PruneMigratedSourceItem,
) -> Result<usize, GraphStoreError> {
    let source_id = VertexId::from(item.source_local_vertex_id);
    let limit = item.bulk_limit as usize;
    let mut handles = Vec::new();
    let mut offset = 0;
    store.skip_then_visit_each_directed_out_edge(source_id, &mut offset, |edge| {
        if !edge.is_tombstone_edge() {
            handles.push((
                LaraLabelId::from_raw(edge.label_id),
                edge.edge_slot_index.raw(),
            ));
        }
        Ok::<bool, GraphStoreError>(handles.len() >= limit)
    })??;
    if handles.len() < limit {
        let mut offset = 0;
        store.skip_then_visit_each_undirected_edge(source_id, &mut offset, |edge| {
            if !edge.is_tombstone_edge() {
                handles.push((
                    LaraLabelId::from_raw(edge.label_id),
                    edge.edge_slot_index.raw(),
                ));
            }
            Ok::<bool, GraphStoreError>(handles.len() >= limit)
        })??;
    }
    let mut removed = 0usize;
    for (label, slot) in handles {
        if store.prune_stub_forward_edge_at_slot(source_id, label, slot)? {
            removed += 1;
        }
    }
    Ok(removed)
}

fn prune_in_reverse_chunk(
    store: &GraphStore,
    item: &mut PruneMigratedSourceItem,
) -> Result<usize, GraphStoreError> {
    let source_id = VertexId::from(item.source_local_vertex_id);
    let limit = item.bulk_limit as usize;
    let mut handles = Vec::new();
    let mut offset = 0;
    store.skip_then_visit_each_directed_in_edge(source_id, &mut offset, |edge| {
        if !edge.is_tombstone_edge() {
            handles.push((
                LaraLabelId::from_raw(edge.label_id),
                edge.edge_slot_index.raw(),
            ));
        }
        Ok::<bool, GraphStoreError>(handles.len() >= limit)
    })??;
    let mut removed = 0usize;
    for (label, slot) in handles {
        if store.prune_stub_reverse_edge_at_slot(source_id, label, slot)? {
            removed += 1;
        }
    }
    Ok(removed)
}

fn clear_stub_vertex_payload(
    store: &GraphStore,
    source_id: VertexId,
) -> Result<(), GraphStoreError> {
    if let Some(vertex) = store.vertex(source_id) {
        let cleared = store
            .set_vertex_labels(source_id, vertex, [])
            .map_err(GraphStoreError::from)?;
        store.set_vertex(source_id, cleared)?;
    }
    for (property_id, _) in store.vertex_properties(source_id) {
        store.remove_vertex_property(source_id, property_id);
    }
    Ok(())
}

fn run_prune_step(
    store: &GraphStore,
    item: &mut PruneMigratedSourceItem,
) -> Result<bool, GraphStoreError> {
    let source_id = VertexId::from(item.source_local_vertex_id);
    if forwarding_stub_epoch(source_id) != Some(item.epoch) {
        remove_prune_item(item.logical_vertex_id);
        return Ok(true);
    }

    match item.phase {
        PruneMigratedSourcePhase::ClearSourceOutEdges => {
            let n = prune_out_edges_chunk(store, item)?;
            item.removed_edges = item.removed_edges.saturating_add(n as u64);
            adjust_prune_bulk_limit(item, n);
            if n == 0 && !stub_out_has_live_edges(store, source_id) {
                item.phase = PruneMigratedSourcePhase::ClearSourceInReverse;
                save_prune_item(item.clone());
            } else {
                save_prune_item(item.clone());
            }
            Ok(false)
        }
        PruneMigratedSourcePhase::ClearSourceInReverse => {
            let n = prune_in_reverse_chunk(store, item)?;
            item.removed_edges = item.removed_edges.saturating_add(n as u64);
            adjust_prune_bulk_limit(item, n);
            if n == 0 && !stub_in_has_live_edges(store, source_id) {
                item.phase = PruneMigratedSourcePhase::ClearSourceVertexPayload;
                save_prune_item(item.clone());
            } else {
                save_prune_item(item.clone());
            }
            Ok(false)
        }
        PruneMigratedSourcePhase::ClearSourceVertexPayload => {
            clear_stub_vertex_payload(store, source_id)?;
            item.phase = PruneMigratedSourcePhase::Done;
            save_prune_item(item.clone());
            Ok(false)
        }
        PruneMigratedSourcePhase::Done => {
            remove_prune_item(item.logical_vertex_id);
            compact_stub_csr(store)?;
            Ok(true)
        }
    }
}

/// Run one prune-maintenance step for the first queued forwarding stub.
pub fn prune_migrated_source_maintenance_step(
    store: &GraphStore,
) -> Result<Option<LogicalVertexId>, GraphStoreError> {
    let Some((logical, mut item)) = PRUNE_MIGRATED_SOURCE_QUEUE.with_borrow(|q| q.first_item())
    else {
        return Ok(None);
    };
    let done = run_prune_step(store, &mut item)?;
    Ok(done.then_some(logical))
}

/// Like [`prune_migrated_source_maintenance_step`] but scoped to one logical vertex.
pub fn prune_migrated_source_maintenance_step_for(
    store: &GraphStore,
    logical_vertex_id: LogicalVertexId,
) -> Result<bool, GraphStoreError> {
    let Some(mut item) = load_prune_item(logical_vertex_id) else {
        return Ok(false);
    };
    run_prune_step(store, &mut item)
}

fn compact_stub_csr(_store: &GraphStore) -> Result<(), GraphStoreError> {
    let _ = GraphStore::compact_lara_graph_after_stub_prune()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn clear_prune_queue_for_test() {
    let stale = PRUNE_MIGRATED_SOURCE_QUEUE.with_borrow(|q| q.logical_ids());
    for logical in stale {
        remove_prune_item(logical);
    }
}
