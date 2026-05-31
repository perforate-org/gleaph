//! Export/import a single vertex during router-coordinated migration.

use super::super::store::{EdgeHandle, GraphStore, GraphStoreError};
use super::index::{remove_source_index_postings_for_vertex, sync_migration_index_postings};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;
use crate::plan::PlanQueryError;
use gleaph_gql::Value;
use gleaph_gql_ic::IcExtensionBinaryDecode;
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, TaggedEdgeLabelId, Vertex};
use gleaph_graph_kernel::federation::RouterError;
use gleaph_graph_kernel::federation::{
    ExportedEdgeTarget, ExportedOutEdge, ExportedProperty, ExportedVertex,
    FinishVertexMigrationArgs, VertexPlacement,
};
use ic_stable_lara::labeled::record::LabeledVertex;
use ic_stable_lara::traits::{CsrEdgeTombstone, CsrVertexTombstone};
use ic_stable_lara::{BucketLabelKey as LaraLabelId, VertexId};

fn catalog_label_from_bucket(bucket: LaraLabelId) -> Option<EdgeLabelId> {
    if bucket == LaraLabelId::UNLABELED_DIRECTED || bucket == LaraLabelId::UNLABELED_UNDIRECTED {
        None
    } else {
        Some(EdgeLabelId::from_raw(bucket.label_index()))
    }
}

fn export_edge_target(
    store: &GraphStore,
    edge: &Edge,
) -> Result<ExportedEdgeTarget, GraphStoreError> {
    match edge.edge_target() {
        Some(gleaph_graph_kernel::entry::EdgeTarget::Local(vid)) => {
            let logical_vertex_id =
                store
                    .logical_vertex_id(vid)
                    .ok_or(GraphStoreError::VertexPlacement(
                        placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
                    ))?;
            Ok(ExportedEdgeTarget::Local { logical_vertex_id })
        }
        Some(gleaph_graph_kernel::entry::EdgeTarget::Remote(remote_ref)) => {
            let logical_vertex_id = store.logical_vertex_for_remote_ref(remote_ref).ok_or(
                GraphStoreError::VertexPlacement(placement::VertexPlacementError::Rejected(
                    RouterError::VertexNotFound,
                )),
            )?;
            Ok(ExportedEdgeTarget::Remote { logical_vertex_id })
        }
        None => Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidArgument(
                "edge without target".into(),
            )),
        )),
    }
}

pub(crate) fn export_out_edge(
    store: &GraphStore,
    owner_vertex_id: VertexId,
    edge: &Edge,
) -> Result<ExportedOutEdge, GraphStoreError> {
    let bucket = store
        .find_forward_edge_bucket_label(owner_vertex_id, edge)?
        .unwrap_or(LaraLabelId::UNLABELED_DIRECTED);
    let undirected = TaggedEdgeLabelId::from_raw(bucket.raw()).is_undirected();
    let catalog_label = catalog_label_from_bucket(bucket);

    let handle = EdgeHandle {
        owner_vertex_id,
        label_id: bucket,
        slot_index: edge.edge_slot_index.raw(),
    };
    let properties = store
        .edge_properties(handle)
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                payload_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property encode: {e}"),
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;

    Ok(ExportedOutEdge {
        catalog_label,
        undirected,
        payload_bytes: edge.payload_bytes().to_vec(),
        target: export_edge_target(store, edge)?,
        properties,
    })
}

/// Serializes a migrating vertex on the source shard (requires `VertexPlacement::Migrating`).
pub fn export_local_vertex_for_migration(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<ExportedVertex, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let vertex = store
        .vertex(vertex_id)
        .filter(|v| !v.is_tombstone())
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;

    let logical_vertex_id =
        store
            .logical_vertex_id(vertex_id)
            .ok_or(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
            ))?;

    #[cfg(not(target_family = "wasm"))]
    let placement = pollster::block_on(placement::resolve_placement(
        routing.router_canister,
        logical_vertex_id,
    ))?;
    #[cfg(target_family = "wasm")]
    let placement = {
        let _ = routing;
        VertexPlacement::Migrating {
            epoch: 0,
            source: gleaph_graph_kernel::federation::PhysicalVertexLocation::new(
                routing.shard_id,
                placement::local_vertex_id_raw(vertex_id),
            ),
            destination_shard_id: 0,
        }
    };
    let VertexPlacement::Migrating { source, .. } = placement else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        ));
    };
    if source.shard_id != routing.shard_id
        || source.local_vertex_id != placement::local_vertex_id_raw(vertex_id)
    {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::Forbidden),
        ));
    }

    let mut vertex_row_bytes = vec![0u8; Vertex::BYTES];
    vertex.into_labeled().write_to(&mut vertex_row_bytes);

    let labels = store.vertex_labels(vertex_id, vertex);
    let properties = store
        .vertex_properties(vertex_id)
        .into_iter()
        .map(|(property_id, value)| {
            Ok(ExportedProperty {
                property_id,
                payload_bytes: value.to_binary_bytes().map_err(|e| {
                    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(
                        format!("property encode: {e}"),
                    ))
                })?,
            })
        })
        .collect::<Result<Vec<_>, GraphStoreError>>()?;

    let mut out_edges = Vec::new();
    for edge in store.directed_out_edges(vertex_id)? {
        if edge.is_tombstone_edge() {
            continue;
        }
        out_edges.push(export_out_edge(store, vertex_id, &edge)?);
    }
    for edge in store.undirected_edges(vertex_id)? {
        if edge.is_tombstone_edge() {
            continue;
        }
        out_edges.push(export_out_edge(store, vertex_id, &edge)?);
    }

    Ok(ExportedVertex {
        logical_vertex_id,
        source_shard_id: source.shard_id,
        source_local_vertex_id: source.local_vertex_id,
        vertex_row_bytes,
        labels,
        properties,
        out_edges,
    })
}

pub(crate) fn import_out_edge(
    store: &GraphStore,
    owner_vertex_id: VertexId,
    edge: &ExportedOutEdge,
) -> Result<(), GraphStoreError> {
    let handle = store.insert_exported_out_edge(
        owner_vertex_id,
        &edge.target,
        edge.undirected,
        &edge.payload_bytes,
        edge.catalog_label,
    )?;

    for prop in &edge.properties {
        let value = Value::from_binary_bytes_with_extensions(
            &prop.payload_bytes,
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

/// Imports vertex data on the destination shard and commits router placement.
pub fn import_migrated_vertex(
    store: &GraphStore,
    bundle: ExportedVertex,
) -> Result<VertexId, GraphStoreError> {
    import_migrated_vertex_impl(store, bundle)
}

/// Like [`import_migrated_vertex`], then updates federated index postings for exported properties.
pub async fn import_migrated_vertex_with_index(
    store: &GraphStore,
    bundle: ExportedVertex,
    index: &dyn PropertyIndexLookup,
) -> Result<VertexId, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;
    let dest_shard = routing.shard_id;
    let vertex_id = import_migrated_vertex_impl(store, bundle.clone())?;
    let dest_local = placement::local_vertex_id_raw(vertex_id);
    sync_migration_index_postings(index, &bundle, dest_shard, dest_local)
        .await
        .map_err(plan_query_to_store)?;
    Ok(vertex_id)
}

fn plan_query_to_store(err: PlanQueryError) -> GraphStoreError {
    GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(err.to_string()))
}

fn import_migrated_vertex_impl(
    store: &GraphStore,
    bundle: ExportedVertex,
) -> Result<VertexId, GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    #[cfg(not(target_family = "wasm"))]
    let placement = pollster::block_on(placement::resolve_placement(
        routing.router_canister,
        bundle.logical_vertex_id,
    ))?;
    #[cfg(target_family = "wasm")]
    let placement = VertexPlacement::Migrating {
        epoch: 0,
        source: gleaph_graph_kernel::federation::PhysicalVertexLocation::new(0, 0),
        destination_shard_id: routing.shard_id,
    };
    let VertexPlacement::Migrating {
        destination_shard_id,
        ..
    } = placement
    else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotMigrating),
        ));
    };
    if destination_shard_id != routing.shard_id {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::Forbidden),
        ));
    }

    if bundle.vertex_row_bytes.len() != Vertex::BYTES {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidArgument(format!(
                "vertex_row_bytes length {}",
                bundle.vertex_row_bytes.len()
            ))),
        ));
    }

    let _source_vertex_row = LabeledVertex::read_from(&bundle.vertex_row_bytes);
    let vertex = Vertex::default();
    let vertex_id = store
        .push_migrated_vertex_row(vertex)
        .map_err(GraphStoreError::from)?;

    store.register_logical_vertex_mapping(vertex_id, bundle.logical_vertex_id);

    let vertex_row = store.vertex(vertex_id).expect("inserted vertex");
    let vertex_row = store
        .set_vertex_labels(vertex_id, vertex_row, bundle.labels)
        .map_err(GraphStoreError::from)?;
    store.set_vertex(vertex_id, vertex_row)?;

    for prop in bundle.properties {
        let value = Value::from_binary_bytes_with_extensions(
            &prop.payload_bytes,
            &IcExtensionBinaryDecode::INSTANCE,
        )
        .map_err(|e| {
            GraphStoreError::VertexPlacement(placement::VertexPlacementError::Call(format!(
                "property decode: {e}"
            )))
        })?;
        store
            .set_vertex_property_without_index_pending(vertex_id, prop.property_id, value)
            .map_err(GraphStoreError::from)?;
    }

    for edge in &bundle.out_edges {
        import_out_edge(store, vertex_id, edge)?;
    }

    #[cfg(not(target_family = "wasm"))]
    pollster::block_on(placement::finish_vertex_migration(
        routing.router_canister,
        FinishVertexMigrationArgs {
            logical_vertex_id: bundle.logical_vertex_id,
            destination_local_vertex_id: placement::local_vertex_id_raw(vertex_id),
        },
    ))?;

    Ok(vertex_id)
}

/// Tombstones the source physical vertex after destination import completes.
pub fn tombstone_migrated_vertex(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<(), GraphStoreError> {
    tombstone_migrated_vertex_impl(store, vertex_id)
}

/// Like [`tombstone_migrated_vertex`], then removes any remaining source-shard index postings.
pub async fn tombstone_migrated_vertex_with_index(
    store: &GraphStore,
    vertex_id: VertexId,
    index: &dyn PropertyIndexLookup,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;
    let source_local = placement::local_vertex_id_raw(vertex_id);
    remove_source_index_postings_for_vertex(
        index,
        store,
        vertex_id,
        routing.shard_id,
        source_local,
    )
    .await
    .map_err(plan_query_to_store)?;
    tombstone_migrated_vertex_impl(store, vertex_id)
}

fn tombstone_migrated_vertex_impl(
    store: &GraphStore,
    vertex_id: VertexId,
) -> Result<(), GraphStoreError> {
    let routing = store
        .federation_routing()
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::ShardNotRegistered),
        ))?;

    let logical_vertex_id =
        store
            .logical_vertex_id(vertex_id)
            .ok_or(GraphStoreError::VertexPlacement(
                placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
            ))?;

    #[cfg(not(target_family = "wasm"))]
    let placement = pollster::block_on(placement::resolve_placement(
        routing.router_canister,
        logical_vertex_id,
    ))?;
    #[cfg(target_family = "wasm")]
    let placement = VertexPlacement::Active(
        gleaph_graph_kernel::federation::PhysicalVertexLocation::new(
            routing.shard_id,
            placement::local_vertex_id_raw(vertex_id),
        ),
    );
    let VertexPlacement::Active(authoritative) = placement else {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "expected Active placement after migration".into(),
            )),
        ));
    };
    if authoritative.shard_id == routing.shard_id
        && authoritative.local_vertex_id == placement::local_vertex_id_raw(vertex_id)
    {
        return Err(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::InvalidMigrationState(
                "vertex is still authoritative on this shard".into(),
            )),
        ));
    }

    let vertex = store
        .vertex(vertex_id)
        .filter(|v| !v.is_tombstone())
        .ok_or(GraphStoreError::VertexPlacement(
            placement::VertexPlacementError::Rejected(RouterError::VertexNotFound),
        ))?;
    store.set_vertex(vertex_id, vertex.with_tombstone(true))?;
    Ok(())
}
