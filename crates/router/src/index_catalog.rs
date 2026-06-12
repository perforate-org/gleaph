//! Router index catalog mutations and shard fan-out (ADR 0009 §4).

use gleaph_graph_kernel::index::{IndexedPropertyKind, RegisterIndexedPropertyArgs};

use crate::facade::stable::ROUTER_INDEXED_PROPERTIES;
use crate::facade::store::RouterStore;
use crate::index_ddl::{IndexDdlStatement, IndexTarget};
use crate::planner_stats::{IndexCatalogEntry, RouterGraphStats};
use crate::state::RouterError;

pub(crate) async fn execute_index_ddl(
    logical_graph_name: &str,
    stmt: IndexDdlStatement,
) -> Result<(), RouterError> {
    match stmt {
        IndexDdlStatement::Create {
            index_name,
            if_not_exists,
            target,
        } => create_index(logical_graph_name, &index_name, if_not_exists, &target).await,
        IndexDdlStatement::Drop {
            index_name,
            if_exists,
        } => drop_index(logical_graph_name, &index_name, if_exists).await,
    }
}

async fn create_index(
    logical_graph_name: &str,
    index_name: &str,
    if_not_exists: bool,
    target: &IndexTarget,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    validate_target_labels(&store, target)?;
    let property_id = store.lookup_property_id(&target.property)?;

    let entry = IndexCatalogEntry {
        kind: target.kind,
        vertex_label: (target.kind == IndexedPropertyKind::Vertex).then(|| target.label.clone()),
        edge_label: (target.kind == IndexedPropertyKind::Edge).then(|| target.label.clone()),
        property: target.property.clone(),
    };

    let newly_registered = ROUTER_INDEXED_PROPERTIES.with_borrow_mut(|m| {
        let stats = m
            .entry(logical_graph_name.to_string())
            .or_insert_with(RouterGraphStats::default);
        stats.create_named_index(index_name, entry, if_not_exists)
    })?;

    if !newly_registered {
        return Ok(());
    }

    fan_out_register(logical_graph_name, target.kind, property_id).await
}

async fn drop_index(
    logical_graph_name: &str,
    index_name: &str,
    if_exists: bool,
) -> Result<(), RouterError> {
    let removed = ROUTER_INDEXED_PROPERTIES.with_borrow_mut(|m| {
        let Some(stats) = m.get_mut(logical_graph_name) else {
            if if_exists {
                return Ok(None);
            }
            return Err(RouterError::NotFound(index_name.to_string()));
        };
        stats.drop_named_index(index_name, if_exists)
    })?;

    let Some((kind, property)) = removed else {
        return Ok(());
    };

    let store = RouterStore::new();
    let property_id = store.lookup_property_id(&property)?;
    let still_indexed = ROUTER_INDEXED_PROPERTIES.with_borrow(|m| {
        m.get(logical_graph_name)
            .is_some_and(|stats| stats.is_property_registered(kind, &property))
    });

    if !still_indexed {
        fan_out_unregister(logical_graph_name, kind, property_id).await?;
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
    logical_graph_name: &str,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    let shards = store.list_shards_for_graph(logical_graph_name)?;
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
    logical_graph_name: &str,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    let shards = store.list_shards_for_graph(logical_graph_name)?;
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
    logical_graph_name: &str,
    kind: IndexedPropertyKind,
    property_id: gleaph_graph_kernel::entry::PropertyId,
) -> Result<(), RouterError> {
    fan_out_register(logical_graph_name, kind, property_id).await
}
