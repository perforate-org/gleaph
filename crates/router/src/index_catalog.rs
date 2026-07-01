//! Router index catalog mutations and shard fan-out (ADR 0009 §4, ADR 0012).

use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId};
use gleaph_graph_kernel::index::IndexedPropertyKind;

use gleaph_graph_kernel::federation::IndexPurgeKind;

use crate::edge_index_direction::direction_tag;
use crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES;
use crate::facade::stable::index_name_catalog::{intern_index_name, lookup_index_name_id};
use crate::facade::stable::indexed_catalog::{
    create_named_index, drop_named_index, edge_index_uses_property_label, is_property_registered,
    load_graph_stats,
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

fn admin_compat_index_name(kind: IndexedPropertyKind, label: &str, property: &str) -> String {
    let kind_tag = match kind {
        IndexedPropertyKind::Vertex => "vertex",
        IndexedPropertyKind::Edge => "edge",
    };
    format!("__gleaph_admin_{kind_tag}_{label}_{property}")
}

/// Legacy admin register → same catalog path as `CREATE INDEX IF NOT EXISTS` (ADR 0009 / 0011).
pub(crate) async fn create_admin_compat_property_index(
    graph_id: GraphId,
    target: IndexTarget,
) -> Result<(), RouterError> {
    let index_name = admin_compat_index_name(target.kind, &target.label, &target.property);
    create_index(graph_id, &index_name, true, &target).await
}

async fn create_index(
    graph_id: GraphId,
    index_name: &str,
    if_not_exists: bool,
    target: &IndexTarget,
) -> Result<(), RouterError> {
    let store = RouterStore::new();
    validate_target_labels(&store, graph_id, target)?;
    let property_id = store.lookup_property_id(graph_id, &target.property)?;
    let label_id = match target.kind {
        IndexedPropertyKind::Vertex => store.lookup_vertex_label_id(graph_id, &target.label)?.raw(),
        IndexedPropertyKind::Edge => store.lookup_edge_label_id(graph_id, &target.label)?.raw(),
    };
    let edge_direction_tag = match target.kind {
        IndexedPropertyKind::Vertex => 0,
        IndexedPropertyKind::Edge => direction_tag(
            target
                .edge_direction
                .expect("edge CREATE INDEX requires direction"),
        ) as u8,
    };

    // Inline/index conflict guard (ADR 0034 Slice 21): an active inline scalar schema for the
    // same (label, property) owns the only valid read source for that property; a sidecar edge
    // index would be semantically stale because payload mutations do not maintain postings.
    if target.kind == IndexedPropertyKind::Edge
        && ROUTER_EDGE_PAYLOAD_PROFILES.with_borrow(|store| {
            store
                .get_record(graph_id, EdgeLabelId::from_raw(label_id))
                .is_some_and(|record| {
                    record.is_inline_scalar() && record.inline_property_id() == Some(property_id)
                })
        })
    {
        return Err(RouterError::Conflict(format!(
            "edge label {} has an inline scalar schema for property {}; drop the inline schema before creating an edge property index",
            target.label, target.property
        )));
    }

    let entry = IndexCatalogEntry {
        kind: target.kind,
        vertex_label: (target.kind == IndexedPropertyKind::Vertex).then(|| target.label.clone()),
        edge_label: (target.kind == IndexedPropertyKind::Edge).then(|| target.label.clone()),
        property: target.property.clone(),
        edge_direction: target.edge_direction,
    };

    let index_name_id = intern_index_name(graph_id, index_name)?;
    // ADR 0023 D1: the router is the sole SSOT for index definitions. Shards learn
    // indexed-ness per operation from the router-supplied catalog, so CREATE INDEX
    // no longer fans out registrations to shards.
    let (_index_inserted, _property_newly_registered) = create_named_index(
        graph_id,
        index_name_id,
        entry,
        property_id,
        label_id,
        edge_direction_tag,
        if_not_exists,
    )?;
    Ok(())
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

    let Some(def) = removed else {
        return Ok(());
    };

    // ADR 0023 D6: DROP INDEX must also purge the dropped property's postings from
    // graph-index (closes P7). Purge only when no remaining index still needs them:
    // - vertex postings carry no label, so they are shared by every vertex index on
    //   the property → purge once the property is fully unregistered.
    // - edge postings carry the catalog label_id → purge the (property, label) scope
    //   once no remaining edge index references it.
    let purge =
        match def.kind {
            IndexedPropertyKind::Vertex => (!is_property_registered(
                graph_id,
                def.kind,
                def.property_id,
            ))
            .then_some((IndexPurgeKind::Vertex, def.property_id.raw(), 0u16)),
            IndexedPropertyKind::Edge => {
                (!edge_index_uses_property_label(graph_id, def.property_id, def.label_id))
                    .then_some((IndexPurgeKind::Edge, def.property_id.raw(), def.label_id))
            }
        };

    if let Some((kind, property_id, label_id)) = purge {
        purge_property_postings(graph_id, kind, property_id, label_id).await?;
    }
    Ok(())
}

/// Fans the bounded posting purge out to every index canister backing `graph_id`'s
/// live shards (ADR 0023 D6). A no-op on native builds (`index_sync` stubs out).
async fn purge_property_postings(
    graph_id: GraphId,
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
) -> Result<(), RouterError> {
    let targets = RouterStore::new().graph_index_lookup_targets(graph_id)?;
    for index_canister in targets {
        crate::index_sync::admin_purge_property_postings(
            index_canister,
            kind,
            property_id,
            label_id,
        )
        .await
        .map_err(RouterError::Internal)?;
    }
    Ok(())
}

fn validate_target_labels(
    store: &RouterStore,
    graph_id: GraphId,
    target: &IndexTarget,
) -> Result<(), RouterError> {
    match target.kind {
        IndexedPropertyKind::Vertex => {
            store.lookup_vertex_label_id(graph_id, &target.label)?;
        }
        IndexedPropertyKind::Edge => {
            store.lookup_edge_label_id(graph_id, &target.label)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::types::EdgeDirection;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
    use gleaph_gql_planner::GraphStats;
    use gleaph_graph_kernel::entry::GraphId;

    fn register_test_graph(store: &RouterStore, name: &str) -> GraphId {
        let owner = candid::Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    graph_name: name.to_owned(),
                    canister_id: candid::Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
            )
            .expect("register graph");
        crate::facade::stable::graph_catalog::lookup_graph_id(name).expect("graph id")
    }

    #[test]
    fn admin_compat_index_name_is_stable() {
        assert_eq!(
            admin_compat_index_name(IndexedPropertyKind::Vertex, "Person", "age"),
            "__gleaph_admin_vertex_Person_age"
        );
    }

    #[test]
    fn admin_compat_create_registers_named_index() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.main");
        store
            .admin_intern_vertex_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.main",
                "Person",
            )
            .expect("intern label");
        store
            .admin_intern_property(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.main",
                "age",
            )
            .expect("intern property");
        futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Vertex,
                label: "Person".into(),
                property: "age".into(),
                edge_direction: None,
            },
        ))
        .expect("create admin compat index");
        let stats = graph_stats_for(graph_id);
        assert!(stats.is_vertex_property_indexed("age"));
        let name = admin_compat_index_name(IndexedPropertyKind::Vertex, "Person", "age");
        assert!(
            crate::facade::stable::index_name_catalog::lookup_index_name_id(graph_id, &name)
                .is_some()
        );
    }

    #[test]
    fn admin_compat_edge_index_uses_any_direction() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.edge");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.edge",
                "KNOWS",
            )
            .expect("intern edge");
        store
            .admin_intern_property(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.edge",
                "weight",
            )
            .expect("intern property");
        futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Edge,
                label: "KNOWS".into(),
                property: "weight".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
        ))
        .expect("create admin compat edge index");
        let stats = graph_stats_for(graph_id);
        assert!(stats.is_edge_property_indexed_for(
            Some("KNOWS"),
            "weight",
            EdgeDirection::PointingRight
        ));
    }
    use crate::facade::stable::edge_payload_profiles::InlineScalarType;

    #[test]
    fn edge_index_create_rejects_inline_scalar_property() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.inline_index_conflict");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.inline_index_conflict",
                "ROAD",
            )
            .expect("intern edge label");
        store
            .admin_intern_property(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.inline_index_conflict",
                "distance",
            )
            .expect("intern property");
        store
            .commit_set_edge_label_inline_scalar_schema(
                graph_id,
                "ROAD",
                "distance",
                InlineScalarType::F32,
            )
            .expect("create inline schema");

        let err = futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Edge,
                label: "ROAD".into(),
                property: "distance".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
        ))
        .expect_err("edge index on inline property should fail");
        assert!(
            matches!(err, RouterError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
    }

    #[test]
    fn inline_schema_create_rejects_existing_edge_index() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.index_inline_conflict");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.index_inline_conflict",
                "ROAD",
            )
            .expect("intern edge label");
        store
            .admin_intern_property(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.index_inline_conflict",
                "distance",
            )
            .expect("intern property");
        futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Edge,
                label: "ROAD".into(),
                property: "distance".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
        ))
        .expect("create edge index");

        let err = store
            .commit_set_edge_label_inline_scalar_schema(
                graph_id,
                "ROAD",
                "distance",
                InlineScalarType::F32,
            )
            .expect_err("inline schema on indexed property should fail");
        assert!(
            matches!(err, RouterError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
    }
}
