//! Federated index posting maintenance during vertex migration.

use crate::facade::GraphStore;
use crate::facade::store::GraphStoreError;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement::VertexPlacementError;
use crate::plan::PlanQueryError;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_ic::IcExtensionBinaryDecode;
use gleaph_graph_kernel::entry::Vertex;
use gleaph_graph_kernel::federation::{
    ExportedProperty, ExportedVertex, MigrationItem, RouterError, ShardId,
};
use ic_stable_lara::VertexId;

fn index_key_bytes(prop: &ExportedProperty) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let value = Value::from_binary_bytes_with_extensions(
        &prop.payload_bytes,
        &IcExtensionBinaryDecode::INSTANCE,
    )
    .map_err(|e| PlanQueryError::FederatedIndexCall {
        op: "migration_property_decode",
        detail: e.to_string(),
    })?;
    Ok(value_to_index_key_bytes(&value).ok().flatten())
}

/// Removes source-shard postings, then inserts destination-shard postings.
///
/// Call after the destination vertex row exists and router placement has been committed.
pub async fn sync_migration_index_postings(
    index: &dyn PropertyIndexLookup,
    bundle: &ExportedVertex,
    destination_shard_id: ShardId,
    destination_local_vertex_id: u32,
) -> Result<(), PlanQueryError> {
    for prop in &bundle.properties {
        let Some(key_bytes) = index_key_bytes(prop)? else {
            continue;
        };
        let pid = prop.property_id.raw();
        index
            .posting_remove_at(
                bundle.source_shard_id,
                pid,
                key_bytes.clone(),
                bundle.source_local_vertex_id,
            )
            .await?;
        index
            .posting_insert_at(
                destination_shard_id,
                pid,
                key_bytes,
                destination_local_vertex_id,
            )
            .await?;
    }
    Ok(())
}

/// Property bundle for [`sync_migration_index_postings`] after destination cutover.
pub fn exported_vertex_for_index_sync(
    store: &GraphStore,
    item: &MigrationItem,
) -> Result<ExportedVertex, GraphStoreError> {
    let dest_id = VertexId::from(item.target_local_vertex_id);
    let vertex = store
        .vertex(dest_id)
        .ok_or(GraphStoreError::VertexPlacement(
            VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;
    let mut vertex_row_bytes = vec![0u8; Vertex::BYTES];
    vertex.into_labeled().write_to(&mut vertex_row_bytes);
    let labels = store.vertex_labels(dest_id, vertex);
    let properties = store
        .vertex_properties(dest_id)
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                payload_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(VertexPlacementError::Call(format!(
                        "property encode: {e}"
                    )))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;
    Ok(ExportedVertex {
        logical_vertex_id: item.logical_vertex_id,
        source_shard_id: item.source_shard_id,
        source_local_vertex_id: item.source_local_vertex_id,
        vertex_row_bytes,
        labels,
        properties,
        out_edges: vec![],
    })
}

/// Best-effort removal of source-shard postings still keyed at a stale physical vertex.
pub async fn remove_source_index_postings_for_vertex(
    index: &dyn PropertyIndexLookup,
    store: &GraphStore,
    vertex_id: VertexId,
    source_shard_id: ShardId,
    source_local_vertex_id: u32,
) -> Result<(), PlanQueryError> {
    for (property_id, value) in store.vertex_properties(vertex_id) {
        let Some(key_bytes) = value_to_index_key_bytes(&value).ok().flatten() else {
            continue;
        };
        index
            .posting_remove_at(
                source_shard_id,
                property_id.raw(),
                key_bytes,
                source_local_vertex_id,
            )
            .await?;
    }
    Ok(())
}
