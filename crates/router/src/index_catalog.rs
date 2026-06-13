//! Router index catalog mutations and shard fan-out (ADR 0009 §4).

use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::index::{IndexedPropertyKind, RegisterIndexedPropertyArgs};

use crate::facade::stable::graph_catalog::lookup_graph_id;
use crate::facade::stable::index_name_catalog::{intern_index_name, lookup_index_name_id};
use crate::facade::stable::indexed_catalog::{
    create_named_index, drop_named_index, is_property_registered, load_graph_stats,
    register_property_membership,
};
use crate::facade::store::RouterStore;
use crate::index_ddl::{IndexDdlStatement, IndexTarget};
use crate::planner_stats::{IndexCatalogEntry, RouterGraphStats};
use crate::state::RouterError;

/// Per-graph indexed property catalog for query planning and DDL.
pub fn graph_stats_for(graph_id: GraphId) -> RouterGraphStats {
    load_graph_stats(graph_id)
}

pub(crate) async fn execute_index_ddl_for_graph(
    graph_id: GraphId,
    stmt: IndexDdlStatement,
) -> Result<(), RouterError> {
    match stmt {
        IndexDdlStatement::Create {
            index_name,
            if_not_exists,
            target,
        } => create_index(graph_id, &index_name, if_not_exists, &target).await,
        IndexDdlStatement::Drop {
            index_name,
            if_exists,
        } => drop_index(graph_id, &index_name, if_exists).await,
    }
}

async fn create_index(
    graph_id: GraphId,
    index_name: &str,
    if_not_exists: bool,
    target: &IndexTarget,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    validate_target_labels(&store, target)?;
    let property_id = store.lookup_property_id(&target.property)?;
    let label_id = match target.kind {
        IndexedPropertyKind::Vertex => store.lookup_vertex_label_id(&target.label)?.raw(),
        IndexedPropertyKind::Edge => store.lookup_edge_label_id(&target.label)?.raw(),
    };

    let entry = IndexCatalogEntry {
        kind: target.kind,
        vertex_label: (target.kind == IndexedPropertyKind::Vertex).then(|| target.label.clone()),
        edge_label: (target.kind == IndexedPropertyKind::Edge).then(|| target.label.clone()),
        property: target.property.clone(),
    };

    let index_name_id = intern_index_name(graph_id, index_name)?;
    let newly_registered = create_named_index(
        graph_id,
        index_name_id,
        entry,
        property_id,
        label_id,
        if_not_exists,
    )?;

    if !newly_registered {
        return Ok(());
    }

    fan_out_register(graph_id, target.kind, property_id).await
}

async fn drop_index(
    graph_id: GraphId,
    index_name: &str,
    if_exists: bool,
) -> Result<(), RouterError> {
    let Some(index_name_id) = lookup_index_name_id(graph_id, index_name) else {
        if if_exists {
            return Ok(());
        }
        return Err(RouterError::NotFound(index_name.to_owned()));
    };
    let removed = drop_named_index(graph_id, index_name_id, if_exists)?;

    let Some((kind, property_id)) = removed else {
        return Ok(());
    };

    if !is_property_registered(graph_id, kind, property_id) {
        fan_out_unregister(graph_id, kind, property_id).await?;
    }
    Ok(())
}

fn validate_target_labels(store: &RouterStore, target: &IndexTarget) -> Result<(), RouterError> {
    match target.kind {
        IndexedPropertyKind::Vertex => {
            store.lookup_vertex_label_id(&target.label)?;
        }
        IndexedPropertyKind::Edge => {
            store.lookup_edge_label_id(&target.label)?;
        }
    }
    Ok(())
}

async fn fan_out_register(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    let graph_name = crate::facade::stable::graph_catalog::graph_name(graph_id)
        .ok_or_else(|| RouterError::NotFound(graph_id.to_string()))?;
    let shards = store.list_shards_for_graph(&graph_name)?;
    let args = RegisterIndexedPropertyArgs {
        kind,
        property_id: property_id.raw(),
    };
    for shard in shards {
        crate::graph_client::register_indexed_property(shard.graph_canister, args)
            .await
            .map_err(RouterError::Internal)?;
    }
    Ok(())
}

async fn fan_out_unregister(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    let graph_name = crate::facade::stable::graph_catalog::graph_name(graph_id)
        .ok_or_else(|| RouterError::NotFound(graph_id.to_string()))?;
    let shards = store.list_shards_for_graph(&graph_name)?;
    let args = RegisterIndexedPropertyArgs {
        kind,
        property_id: property_id.raw(),
    };
    for shard in shards {
        crate::graph_client::unregister_indexed_property(shard.graph_canister, args)
            .await
            .map_err(RouterError::Internal)?;
    }
    Ok(())
}

pub(crate) async fn register_indexed_property_on_shards(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), RouterError> {
    fan_out_register(graph_id, kind, property_id).await
}

pub(crate) fn register_property_membership_if_absent(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> bool {
    register_property_membership(graph_id, kind, property_id)
}
