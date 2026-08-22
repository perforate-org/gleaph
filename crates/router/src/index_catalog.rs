//! Router index catalog mutations and shard fan-out (ADR 0009 §4, ADR 0012).

use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId};
use gleaph_graph_kernel::index::IndexedPropertyKind;

use gleaph_graph_kernel::federation::IndexPurgeKind;

use crate::edge_index_direction::to_index_direction;
use crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES;
use crate::facade::stable::embedding_name_catalog::{
    intern_embedding_name, lookup_embedding_name_id, preflight_embedding_name,
};
use crate::facade::stable::index_name_catalog::{
    intern_index_name, lookup_index_name_id, preflight_index_name,
};
use crate::facade::stable::indexed_catalog::{
    create_named_index, drop_named_index, get_named_index, load_graph_stats,
};
use crate::facade::stable::vector_index_catalog;
use crate::facade::store::RouterStore;
use crate::index_ddl::{IndexDdlStatement, IndexTarget, VectorIndexDdlStatement};
use crate::planner_stats::{IndexCatalogEntry, RouterGraphStats};
use crate::state::RouterError;

/// Per-graph planner projection derived exclusively from `Active` physical-index lifecycle rows.
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

/// Execute Router-owned semantic vector DDL. In provisioned mode (a `provision_canister` is
/// configured) a `CREATE VECTOR INDEX` on a graph with no vector target provisions a vector
/// canister first, then registers the definition against that target; otherwise it commits one
/// targetless `Registered` definition (dev mode).
pub(crate) async fn execute_vector_index_ddl_for_graph(
    graph_id: GraphId,
    stmt: VectorIndexDdlStatement,
) -> Result<(), RouterError> {
    match stmt {
        VectorIndexDdlStatement::Create {
            index_name,
            if_not_exists,
            target,
        } => create_vector_index(graph_id, &index_name, if_not_exists, &target).await,
    }
}

/// Reject a new vector registration whose logical name is already owned by either index kind.
/// Legacy admin registration calls this before interning its embedding/name compatibility alias.
pub(crate) fn ensure_vector_index_name_available(
    graph_id: GraphId,
    index_name: &str,
) -> Result<(), RouterError> {
    let Some(index_name_id) = lookup_index_name_id(graph_id, index_name) else {
        return Ok(());
    };
    if get_named_index(graph_id, index_name_id).is_some()
        || vector_index_catalog::get_vector_index_by_name_id(graph_id, index_name_id).is_some()
    {
        return Err(RouterError::Conflict(format!(
            "logical index name is already registered: {index_name}"
        )));
    }
    Ok(())
}

async fn create_vector_index(
    graph_id: GraphId,
    index_name: &str,
    if_not_exists: bool,
    target: &crate::index_ddl::VectorIndexTarget,
) -> Result<(), RouterError> {
    if index_name.is_empty() || target.embedding_name.is_empty() {
        return Err(RouterError::InvalidArgument(
            "vector index and embedding field names must not be empty".into(),
        ));
    }
    if target.dims == 0 {
        return Err(RouterError::InvalidArgument(
            "vector index dimensions must be positive".into(),
        ));
    }

    let store = RouterStore::new();

    // In provisioned mode, the graph may not yet have a vector canister. If none is set, provision
    // one before registering the definition so the created index has a dispatch target.
    let provisioned_target = if crate::provisioning::config::get().is_some()
        && vector_index_catalog::graph_single_target(graph_id).is_none()
    {
        Some(provision_vector_canister(graph_id).await?)
    } else {
        vector_index_catalog::graph_single_target(graph_id)
    };
    let label_id = store.lookup_vertex_label_id(graph_id, &target.label)?;
    let existing_index_name_id = lookup_index_name_id(graph_id, index_name);
    let existing_embedding_name_id = lookup_embedding_name_id(graph_id, &target.embedding_name);

    if let Some(index_name_id) = existing_index_name_id {
        if get_named_index(graph_id, index_name_id).is_some() {
            return Err(RouterError::Conflict(format!(
                "index name already belongs to a property index: {index_name}"
            )));
        }
        if let Some(existing) =
            vector_index_catalog::get_vector_index_by_name_id(graph_id, index_name_id)
        {
            let exact = existing_embedding_name_id == Some(existing.embedding_name_id)
                && existing.labels == [label_id]
                && existing.kind == target.kind
                && existing.metric == target.metric
                && existing.encoding == target.encoding
                && existing.dims == target.dims;
            if exact && if_not_exists {
                return Ok(());
            }
            return Err(RouterError::Conflict(format!(
                "vector index definition already exists with a different declaration: {index_name}"
            )));
        }
    }

    if let Some(embedding_name_id) = existing_embedding_name_id
        && vector_index_catalog::get_vector_index_by_embedding_name_id(graph_id, embedding_name_id)
            .is_some()
    {
        return Err(RouterError::Conflict(format!(
            "embedding field already has a vector index: {}",
            target.embedding_name
        )));
    }

    // Prove every fallible allocation before the first durable write. The following commit region
    // is synchronous and contains no recoverable validation or remote effect.
    preflight_index_name(graph_id, index_name)?;
    preflight_embedding_name(graph_id, &target.embedding_name)?;
    vector_index_catalog::preflight_allocate_vector_index_id()?;

    let index_name_id = intern_index_name(graph_id, index_name)?;
    let embedding_name_id = intern_embedding_name(graph_id, &target.embedding_name)?;
    let index_id = vector_index_catalog::allocate_vector_index_id()?;
    vector_index_catalog::register_vector_index(
        graph_id,
        index_id,
        index_name_id,
        embedding_name_id,
        vec![label_id],
        target.kind,
        target.metric,
        target.encoding,
        target.dims,
        provisioned_target.map(|canister| vector_index_catalog::VectorIndexTarget { canister }),
        false,
    )?;
    Ok(())
}

/// Provision a vector canister for a graph that currently has no vector target, and return the
/// created canister principal. Uses the shared provisioned graph-admission flow with a single
/// `VectorIndex(0)` resource (the whole-graph target, ADR 0031 Slice 4). Only callable when a
/// `provision_canister` is configured.
async fn provision_vector_canister(graph_id: GraphId) -> Result<candid::Principal, RouterError> {
    use gleaph_graph_kernel::federation::VectorIndexId;
    use gleaph_graph_kernel::provisioning::LogicalResource;
    use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;

    let caller = ic_cdk::api::msg_caller();
    let graph_name = crate::facade::stable::graph_catalog::graph_name(graph_id)
        .ok_or_else(|| RouterError::NotFound("graph not found".to_owned()))?;
    let args = crate::types::ProvisionGraphArgs {
        deployment_id: caller.to_text(),
        graph_name,
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::VectorIndex(VectorIndexId::new(0)),
        }],
        authorized_caller: caller,
        release_id: "default".to_owned(),
        owner: caller,
        admins: std::collections::BTreeSet::new(),
    };
    let response = crate::provisioning::graph::provision_resource_flow(caller, args).await?;
    match response {
        crate::types::ProvisionGraphResponse::Accepted {
            created_resources, ..
        } => created_resources
            .into_iter()
            .find(|r| matches!(r.logical_resource, LogicalResource::VectorIndex(_)))
            .map(|r| r.canister_id)
            .ok_or_else(|| {
                RouterError::Internal(
                    "provisioned vector canister missing from created_resources".to_owned(),
                )
            }),
        crate::types::ProvisionGraphResponse::Replay { .. }
        | crate::types::ProvisionGraphResponse::Completed => {
            // A replayed/acknowledged admission means a prior call already provisioned the canister;
            // resolve it from the catalog.
            vector_index_catalog::graph_single_target(graph_id).ok_or_else(|| {
                RouterError::Internal("vector target not set after provision".to_owned())
            })
        }
    }
}

pub(crate) struct ResolvedIndexDefinition {
    pub entry: IndexCatalogEntry,
    pub property_id: gleaph_graph_kernel::entry::PropertyId,
    pub label_id: u16,
    pub edge_direction: Option<gleaph_graph_kernel::index::EdgeIndexDirection>,
}

/// Resolve one logical CREATE INDEX target without allocating a name, physical namespace, or
/// catalog row. Immediate DDL and migration preflight share this validation owner.
pub(crate) fn resolve_index_definition(
    graph_id: GraphId,
    target: &IndexTarget,
) -> Result<ResolvedIndexDefinition, RouterError> {
    let store = RouterStore::new();
    validate_target_labels(&store, graph_id, target)?;
    let property_id = store.lookup_property_id(graph_id, &target.property)?;
    let label_id = match target.kind {
        IndexedPropertyKind::Vertex => store.lookup_vertex_label_id(graph_id, &target.label)?.raw(),
        IndexedPropertyKind::Edge => store.lookup_edge_label_id(graph_id, &target.label)?.raw(),
    };
    let edge_direction = match target.kind {
        IndexedPropertyKind::Vertex => None,
        IndexedPropertyKind::Edge => Some(to_index_direction(
            target
                .edge_direction
                .expect("edge CREATE INDEX requires direction"),
        )),
    };

    if target.kind == IndexedPropertyKind::Edge
        && ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|profiles| {
            profiles
                .get_record(graph_id, EdgeLabelId::from_raw(label_id))
                .is_some_and(|record| {
                    record.is_inline_struct() && record.inline_property_id() == Some(property_id)
                })
        })
    {
        return Err(RouterError::Conflict(format!(
            "edge label {} has an inline struct property {}; index a leaf field instead",
            target.label, target.property
        )));
    }
    if target.kind == IndexedPropertyKind::Edge && target.property.contains('.') {
        let (top, _) = target
            .property
            .split_once('.')
            .expect("contains dot implies split_once");
        let top_id = store.lookup_property_id(graph_id, top)?;
        let is_field = ROUTER_EDGE_INLINE_PROPERTY_PROFILES.with_borrow(|profiles| {
            profiles
                .get_record(graph_id, EdgeLabelId::from_raw(label_id))
                .is_some_and(|record| {
                    record.is_inline_struct()
                        && record.inline_property_id() == Some(top_id)
                        && record.inline_struct_field_paths().iter().any(|path| {
                            path == &target.property || format!("{top}.{path}") == target.property
                        })
                })
        });
        if !is_field {
            return Err(RouterError::InvalidArgument(format!(
                "edge index property {} is not an inline struct leaf on label {}",
                target.property, target.label
            )));
        }
    }

    Ok(ResolvedIndexDefinition {
        entry: IndexCatalogEntry {
            kind: target.kind,
            vertex_label: (target.kind == IndexedPropertyKind::Vertex)
                .then(|| target.label.clone()),
            edge_label: (target.kind == IndexedPropertyKind::Edge).then(|| target.label.clone()),
            property: target.property.clone(),
            edge_direction: target.edge_direction,
        },
        property_id,
        label_id,
        edge_direction,
    })
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
    let resolved = resolve_index_definition(graph_id, target)?;

    if let Some(index_name_id) = lookup_index_name_id(graph_id, index_name)
        && vector_index_catalog::get_vector_index_by_name_id(graph_id, index_name_id).is_some()
    {
        return Err(RouterError::Conflict(format!(
            "index name already belongs to a vector index: {index_name}"
        )));
    }

    // Defer to `create_named_index`'s if_not_exists semantics without provisioning canisters for a
    // no-op or a rejected re-declaration.
    if let Some(index_name_id) = lookup_index_name_id(graph_id, index_name)
        && get_named_index(graph_id, index_name_id).is_some()
    {
        if if_not_exists {
            return Ok(());
        }
        return Err(RouterError::Conflict(format!(
            "index already exists: {index_name}"
        )));
    }

    // In provisioned mode, ensure every live shard group has an index canister before registering
    // the definition so the created index has a dispatch target (ADR 0035 Slice 10).
    ensure_index_canisters_provisioned(graph_id).await?;

    let index_name_id = intern_index_name(graph_id, index_name)?;
    // ADR 0023 D1: the router is the sole SSOT for index definitions. Shards learn
    // indexed-ness per operation from the router-supplied catalog, so CREATE INDEX
    // no longer fans out registrations to shards.
    // ADR 0059 foundation bridge: ordinary CREATE INDEX preserves ADR 0009's immediate publication
    // contract. Migration CREATE INDEX will enter through prepare/build/seal instead.
    let _outcome = create_named_index(
        graph_id,
        index_name_id,
        resolved.entry,
        resolved.property_id,
        resolved.label_id,
        resolved.edge_direction,
        if_not_exists,
    )?;
    Ok(())
}

/// In provisioned mode, ensure every live shard group of `graph_id` has an index canister
/// assigned. Unassigned groups are provisioned through the shared admission flow and their
/// canisters assigned to `index_cluster` + retrofit-attached to the group's shards. Dev mode (no
/// `provision_canister`) is a no-op (indexless).
async fn ensure_index_canisters_provisioned(graph_id: GraphId) -> Result<(), RouterError> {
    if crate::provisioning::config::get().is_none() {
        return Ok(());
    }
    let store = RouterStore::new();
    let unassigned = store.unassigned_index_groups(graph_id)?;
    if unassigned.is_empty() {
        return Ok(());
    }
    let canisters = provision_index_canisters(graph_id, &unassigned).await?;
    store
        .attach_provisioned_index_canisters(graph_id, &canisters)
        .await
}

/// Provision index canisters for the given shard groups through the shared admission flow and
/// return `group_index -> canister`. Only callable when a `provision_canister` is configured.
async fn provision_index_canisters(
    graph_id: GraphId,
    groups: &[u32],
) -> Result<std::collections::BTreeMap<u32, candid::Principal>, RouterError> {
    use gleaph_graph_kernel::federation::IndexClusterId;
    use gleaph_graph_kernel::provisioning::LogicalResource;
    use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;

    let caller = ic_cdk::api::msg_caller();
    let graph_name = crate::facade::stable::graph_catalog::graph_name(graph_id)
        .ok_or_else(|| RouterError::NotFound("graph not found".to_owned()))?;
    let args = crate::types::ProvisionGraphArgs {
        deployment_id: caller.to_text(),
        graph_name,
        requested_resources: groups
            .iter()
            .map(|g| ProvisionableResource {
                logical_resource: LogicalResource::PropertyIndex(IndexClusterId::new(*g)),
            })
            .collect(),
        authorized_caller: caller,
        release_id: "default".to_owned(),
        owner: caller,
        admins: std::collections::BTreeSet::new(),
    };
    let response = crate::provisioning::graph::provision_resource_flow(caller, args).await?;
    match response {
        crate::types::ProvisionGraphResponse::Accepted {
            created_resources, ..
        }
        | crate::types::ProvisionGraphResponse::Replay {
            created_resources, ..
        } => {
            let mut out = std::collections::BTreeMap::new();
            for r in created_resources {
                if let LogicalResource::PropertyIndex(cluster) = r.logical_resource {
                    out.insert(cluster.raw(), r.canister_id);
                }
            }
            for g in groups {
                if !out.contains_key(g) {
                    return Err(RouterError::Internal(format!(
                        "provisioned index canister for group {g} missing from created_resources"
                    )));
                }
            }
            Ok(out)
        }
        crate::types::ProvisionGraphResponse::Completed => {
            // A replayed/acknowledged admission means a prior call already provisioned and
            // assigned the canisters; resolve them from the graph's index_cluster.
            let cluster = RouterStore::new().graph_index_cluster(graph_id)?;
            groups
                .iter()
                .map(|g| {
                    let canister = *cluster.get(*g as usize).ok_or_else(|| {
                        RouterError::Internal(format!(
                            "index canister for group {g} missing after provision"
                        ))
                    })?;
                    Ok((*g, canister))
                })
                .collect()
        }
    }
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
    // Preflight BEFORE the destructive catalog mutation: freeze the graph's live index
    // canisters as the retirement's drain targets so a registry error rejects the DDL
    // before any state is removed.
    let targets = RouterStore::new().graph_index_lookup_targets(graph_id)?;
    let removed = drop_named_index(graph_id, index_name_id, if_exists)?;

    let Some(def) = removed else {
        return Ok(());
    };

    // ADR 0023 D6 (+ GAP-2026-08-20-005): co-write the catalog removal with a durable
    // retirement record keyed by PhysicalIndexId. Postings are namespaced by physical id
    // and no reader can reach a namespace whose catalog row is gone, so EVERY dropped
    // definition retires its own namespace unconditionally — a sibling index on the same
    // property neither needs those postings nor protects them. The record — not this
    // call — owns purge progress from here on: a response loss, a stopped target, or an
    // upgrade defers the remainder to the recovery lane instead of orphaning them.
    let kind = match def.kind {
        IndexedPropertyKind::Vertex => IndexPurgeKind::Vertex,
        IndexedPropertyKind::Edge => IndexPurgeKind::Edge,
    };
    // A graph with no live index canister owns its posting cleanup through the shard
    // detach flow instead; an empty fan-out would enqueue a self-deleting record.
    if !targets.is_empty() {
        crate::facade::stable::index_retirement::enqueue_retirement(
            def.physical_index_id,
            crate::facade::stable::index_retirement::RetiredIndexRecord {
                graph_id,
                kind,
                property_id: def.property_id.raw(),
                label_id: def.label_id,
                pending: targets
                    .iter()
                    .map(|canister| {
                        crate::facade::stable::index_retirement::RetirementTargetDrain {
                            canister: *canister,
                            resume: None,
                        }
                    })
                    .collect(),
                enqueued_at_ns: crate::facade::store::ic_time_ns(),
            },
        );
        // Arm the autonomous lane first so even a trapped inline drain converges without a caller.
        crate::recovery::arm_if_needed();
        // Inline fast path: bounded rounds until done or hold; holds defer to the recovery lane.
        crate::index_retirement::drain_retirement_inline(def.physical_index_id).await;
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
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorIndexKind, VectorMetric};
    use gleaph_index_ddl::VectorIndexDdlParseError;

    fn register_test_graph(store: &RouterStore, name: &str) -> GraphId {
        let owner = candid::Principal::from_slice(&[1; 29]);
        crate::facade::auth::grant_admins(&[owner]);
        store
            .admin_register_graph(
                owner,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: candid::Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: false,
                },
                name,
            )
            .expect("register graph");
        crate::facade::stable::graph_catalog::lookup_graph_id(name).expect("graph id")
    }

    fn register_vector_label(store: &RouterStore, graph_name: &str, label: &str) {
        store
            .admin_intern_vertex_label(candid::Principal::from_slice(&[1; 29]), graph_name, label)
            .expect("intern vector label");
    }

    fn vector_statement(
        index_name: &str,
        embedding_name: &str,
        dims: u16,
        if_not_exists: bool,
    ) -> VectorIndexDdlStatement {
        VectorIndexDdlStatement::Create {
            index_name: index_name.into(),
            if_not_exists,
            target: crate::index_ddl::VectorIndexTarget {
                label: "Document".into(),
                embedding_name: embedding_name.into(),
                dims,
                metric: VectorMetric::Cosine,
                encoding: VectorEncoding::F32,
                kind: VectorIndexKind::IvfFlat,
            },
        }
    }

    #[test]
    fn vector_ddl_registers_distinct_names_as_targetless_registered() {
        let store = RouterStore::new();
        let graph_name = "tenant.vector.ddl";
        let graph_id = register_test_graph(&store, graph_name);
        register_vector_label(&store, graph_name, "Document");

        futures::executor::block_on(execute_vector_index_ddl_for_graph(
            graph_id,
            vector_statement("document_embedding_idx", "embedding", 768, false),
        ))
        .expect("create vector index");

        let index_name_id =
            lookup_index_name_id(graph_id, "document_embedding_idx").expect("logical index name");
        let embedding_name_id =
            lookup_embedding_name_id(graph_id, "embedding").expect("embedding field name");
        let def = vector_index_catalog::get_vector_index_by_name_id(graph_id, index_name_id)
            .expect("vector definition");
        assert_eq!(def.index_name_id, index_name_id);
        assert_eq!(def.embedding_name_id, embedding_name_id);
        assert_eq!(def.labels.len(), 1);
        assert_eq!(def.dims, 768);
        assert!(def.target.is_none());
        assert_eq!(
            def.activation_state,
            vector_index_catalog::VectorIndexActivationState::Registered
        );
    }

    #[test]
    fn vector_ddl_if_not_exists_requires_exact_definition_without_side_effects() {
        let store = RouterStore::new();
        let graph_name = "tenant.vector.replay";
        let graph_id = register_test_graph(&store, graph_name);
        register_vector_label(&store, graph_name, "Document");

        futures::executor::block_on(execute_vector_index_ddl_for_graph(
            graph_id,
            vector_statement("document_embedding_idx", "embedding", 384, false),
        ))
        .expect("initial create");
        let next_before =
            crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get());

        futures::executor::block_on(execute_vector_index_ddl_for_graph(
            graph_id,
            vector_statement("document_embedding_idx", "embedding", 384, true),
        ))
        .expect("exact replay");
        assert_eq!(
            crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_before,
            "exact replay must not allocate a physical id"
        );

        let err = futures::executor::block_on(execute_vector_index_ddl_for_graph(
            graph_id,
            vector_statement("document_embedding_idx", "other_embedding", 385, true),
        ))
        .expect_err("shape mismatch must conflict");
        assert!(matches!(err, RouterError::Conflict(_)));
        assert_eq!(
            lookup_embedding_name_id(graph_id, "other_embedding"),
            None,
            "conflict must not allocate an embedding field name"
        );
        assert_eq!(
            crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_before,
            "conflict must not allocate a physical id"
        );

        register_vector_label(&store, graph_name, "OtherDocument");
        let base = crate::index_ddl::VectorIndexTarget {
            label: "Document".into(),
            embedding_name: "embedding".into(),
            dims: 384,
            metric: VectorMetric::Cosine,
            encoding: VectorEncoding::F32,
            kind: VectorIndexKind::IvfFlat,
        };
        let mut label_mismatch = base.clone();
        label_mismatch.label = "OtherDocument".into();
        let mut embedding_mismatch = base.clone();
        embedding_mismatch.embedding_name = "other_embedding_single".into();
        let mut dims_mismatch = base.clone();
        dims_mismatch.dims = 385;
        let mut metric_mismatch = base.clone();
        metric_mismatch.metric = VectorMetric::L2Squared;
        let mut encoding_mismatch = base;
        encoding_mismatch.encoding = VectorEncoding::I8;
        let original = vector_index_catalog::get_vector_index_by_name_id(
            graph_id,
            lookup_index_name_id(graph_id, "document_embedding_idx").expect("index name"),
        )
        .expect("original definition");
        for (field, target) in [
            ("label", label_mismatch),
            ("embedding", embedding_mismatch),
            ("dimensions", dims_mismatch),
            ("metric", metric_mismatch),
            ("encoding", encoding_mismatch),
        ] {
            let result = futures::executor::block_on(execute_vector_index_ddl_for_graph(
                graph_id,
                VectorIndexDdlStatement::Create {
                    index_name: "document_embedding_idx".into(),
                    if_not_exists: true,
                    target,
                },
            ));
            assert!(
                matches!(result, Err(RouterError::Conflict(_))),
                "single-field {field} mismatch must conflict: {result:?}"
            );
            assert_eq!(
                vector_index_catalog::get_vector_index(graph_id, original.index_id),
                Some(original.clone()),
                "field={field}: original definition must survive"
            );
            assert_eq!(
                crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get()),
                next_before,
                "field={field}: allocator must not advance"
            );
            assert_eq!(
                lookup_index_name_id(graph_id, "document_embedding_idx"),
                Some(original.index_name_id),
                "field={field}: logical name mapping must remain canonical"
            );
            assert_eq!(
                lookup_embedding_name_id(graph_id, "embedding"),
                Some(original.embedding_name_id),
                "field={field}: embedding name mapping must remain canonical"
            );
        }
        assert_eq!(
            lookup_embedding_name_id(graph_id, "other_embedding_single"),
            None,
            "embedding mismatch must not allocate its candidate name"
        );

        let invalid_kind = crate::index_ddl::try_parse_vector(
            r#"CREATE VECTOR INDEX document_embedding_idx IF NOT EXISTS
               FOR (d:Document) ON d.embedding
               OPTIONS { dimensions: 384, metric: "cosine", encoding: "f32", algorithm: "hnsw" }"#,
        )
        .expect("vector DDL detection")
        .expect_err("unsupported kind must be rejected before Router registration");
        assert!(matches!(
            invalid_kind,
            VectorIndexDdlParseError::InvalidOptionValue { .. }
        ));
        assert_eq!(
            vector_index_catalog::get_vector_index(graph_id, original.index_id),
            Some(original)
        );
        assert_eq!(
            crate::facade::stable::ROUTER_NEXT_VECTOR_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_before,
            "unsupported kind must not allocate"
        );
    }

    #[test]
    fn vector_registration_rejects_property_index_logical_name_collision() {
        let store = RouterStore::new();
        let graph_name = "tenant.vector.name_collision";
        let graph_id = register_test_graph(&store, graph_name);
        register_vector_label(&store, graph_name, "Document");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            graph_name,
            "title",
        );
        let name = "embedding";
        futures::executor::block_on(create_index(
            graph_id,
            name,
            false,
            &IndexTarget {
                kind: IndexedPropertyKind::Vertex,
                label: "Document".into(),
                property: "title".into(),
                edge_direction: None,
            },
        ))
        .expect("create property index");
        let err = ensure_vector_index_name_available(graph_id, name)
            .expect_err("admin compatibility name must preflight before interning");
        assert!(matches!(err, RouterError::Conflict(_)));
        assert_eq!(
            lookup_embedding_name_id(graph_id, name),
            None,
            "preflight conflict must not allocate an embedding name"
        );
        let index_name_id = lookup_index_name_id(graph_id, name).expect("property index name");
        let embedding_name_id = intern_embedding_name(graph_id, "embedding")
            .expect("intern embedding name for direct registration");

        let err = vector_index_catalog::register_vector_index(
            graph_id,
            77,
            index_name_id,
            embedding_name_id,
            vec![
                store
                    .lookup_vertex_label_id(graph_id, "Document")
                    .expect("label"),
            ],
            VectorIndexKind::IvfFlat,
            VectorMetric::Cosine,
            VectorEncoding::F32,
            16,
            None,
            false,
        )
        .expect_err("cross-kind logical name must conflict");
        assert!(matches!(err, RouterError::Conflict(_)));
        assert!(
            vector_index_catalog::get_vector_index(graph_id, 77).is_none(),
            "rejected registration must not persist a vector definition"
        );
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
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.main",
            "age",
        );
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
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.edge",
            "weight",
        );
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
    use crate::facade::stable::edge_inline_property_profiles::InlineScalarType;

    #[test]
    fn edge_index_create_allows_inline_scalar_property() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.inline_index_conflict");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.inline_index_conflict",
                "ROAD",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.inline_index_conflict",
            "distance",
        );
        store
            .commit_set_edge_label_inline_scalar_schema(
                graph_id,
                "ROAD",
                "distance",
                InlineScalarType::F32,
            )
            .expect("create inline schema");

        futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Edge,
                label: "ROAD".into(),
                property: "distance".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
        ))
        .expect("edge index on inline scalar property");
        assert!(graph_stats_for(graph_id).is_edge_property_indexed_for(
            Some("ROAD"),
            "distance",
            EdgeDirection::PointingRight,
        ));
    }

    #[test]
    fn inline_schema_create_allows_existing_edge_index() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.index_inline_conflict");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.index_inline_conflict",
                "ROAD",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.index_inline_conflict",
            "distance",
        );
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

        store
            .commit_set_edge_label_inline_scalar_schema(
                graph_id,
                "ROAD",
                "distance",
                InlineScalarType::F32,
            )
            .expect("inline schema on indexed scalar property");
    }

    #[test]
    fn edge_index_create_rejects_existing_inline_struct() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.inline_struct_index_conflict");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.inline_struct_index_conflict",
                "AFFINITY",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.inline_struct_index_conflict",
            "stats",
        );
        store
            .commit_set_edge_label_inline_struct_schema(
                graph_id,
                "AFFINITY",
                "stats",
                vec![
                    crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                        name: "score".into(),
                        scalar_type: InlineScalarType::F32,
                    },
                ],
            )
            .expect("create inline struct schema");

        let err = futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Edge,
                label: "AFFINITY".into(),
                property: "stats".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
        ))
        .expect_err("edge index on inline struct property should fail");
        assert!(
            matches!(err, RouterError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
    }

    #[test]
    fn inline_struct_create_rejects_existing_edge_index() {
        let store = RouterStore::new();
        let graph_id = register_test_graph(&store, "tenant.index_inline_struct_conflict");
        store
            .admin_intern_edge_label(
                candid::Principal::from_slice(&[1; 29]),
                "tenant.index_inline_struct_conflict",
                "AFFINITY",
            )
            .expect("intern edge label");
        crate::facade::store::catalog_test_support::intern_property(
            &store,
            candid::Principal::from_slice(&[1; 29]),
            "tenant.index_inline_struct_conflict",
            "stats",
        );
        futures::executor::block_on(create_admin_compat_property_index(
            graph_id,
            IndexTarget {
                kind: IndexedPropertyKind::Edge,
                label: "AFFINITY".into(),
                property: "stats".into(),
                edge_direction: Some(EdgeDirection::AnyDirection),
            },
        ))
        .expect("create edge index");

        let err = store
            .commit_set_edge_label_inline_struct_schema(
                graph_id,
                "AFFINITY",
                "stats",
                vec![
                    crate::facade::stable::edge_inline_property_profiles::InlineStructFieldSpec {
                        name: "score".into(),
                        scalar_type: InlineScalarType::F32,
                    },
                ],
            )
            .expect_err("inline struct on indexed property should fail");
        assert!(
            matches!(err, RouterError::Conflict(_)),
            "expected Conflict, got {err:?}"
        );
    }

    #[test]
    fn vector_ddl_new_definition_inherits_existing_graph_target() {
        let store = RouterStore::new();
        let graph_name = "tenant.vector.target";
        let graph_id = register_test_graph(&store, graph_name);
        register_vector_label(&store, graph_name, "Document");

        // First definition is targetless (dev mode, no provision_canister, no prior target).
        futures::executor::block_on(execute_vector_index_ddl_for_graph(
            graph_id,
            vector_statement("first_idx", "embedding", 384, false),
        ))
        .expect("first create");
        let first_idx = lookup_index_name_id(graph_id, "first_idx").expect("first index name");
        let first_def = vector_index_catalog::get_vector_index_by_name_id(graph_id, first_idx)
            .expect("first def");
        assert!(first_def.target.is_none());

        // Set the graph's single target to a concrete canister.
        let target_canister = candid::Principal::from_slice(&[0x50; 29]);
        vector_index_catalog::set_vector_index_target(
            graph_id,
            first_def.index_id,
            vector_index_catalog::VectorIndexTarget {
                canister: target_canister,
            },
        )
        .expect("set target");
        assert_eq!(
            vector_index_catalog::graph_single_target(graph_id),
            Some(target_canister)
        );

        // A second definition in the same graph inherits the graph's single target.
        register_vector_label(&store, graph_name, "OtherDocument");
        futures::executor::block_on(execute_vector_index_ddl_for_graph(
            graph_id,
            vector_statement("second_idx", "embedding2", 384, false),
        ))
        .expect("second create");
        let second_idx = lookup_index_name_id(graph_id, "second_idx").expect("second index name");
        let second_def = vector_index_catalog::get_vector_index_by_name_id(graph_id, second_idx)
            .expect("second def");
        assert_eq!(
            second_def.target.map(|t| t.canister),
            Some(target_canister),
            "new definition must inherit the graph's single target"
        );
    }

    fn register_indexless_shard(
        store: &RouterStore,
        admin: candid::Principal,
        graph_name: &str,
        shard_id: u32,
        graph_byte: u8,
    ) {
        futures::executor::block_on(store.admin_register_shard(
            admin,
            crate::types::AdminRegisterShardArgs {
                shard_id: gleaph_graph_kernel::federation::ShardId::new(shard_id),
                graph_canister: candid::Principal::from_slice(&[graph_byte; 29]),
                index_canister: candid::Principal::anonymous(),
                logical_graph_name: graph_name.into(),
            },
        ))
        .expect("register indexless shard");
    }

    #[test]
    fn create_index_dev_mode_leaves_graph_indexless() {
        let store = RouterStore::new();
        let graph_name = "tenant.indexless.dev";
        let graph_id = register_test_graph(&store, graph_name);
        let admin = candid::Principal::from_slice(&[1; 29]);
        store
            .admin_intern_vertex_label(admin, graph_name, "Person")
            .expect("intern label");
        crate::facade::store::catalog_test_support::intern_property(
            &store, admin, graph_name, "age",
        );
        register_indexless_shard(&store, admin, graph_name, 0, 0x50);

        // No provision_canister configured (dev mode): CREATE INDEX must not assign an index
        // canister; the graph stays indexless.
        futures::executor::block_on(create_index(
            graph_id,
            "age_idx",
            false,
            &IndexTarget {
                kind: IndexedPropertyKind::Vertex,
                label: "Person".into(),
                property: "age".into(),
                edge_direction: None,
            },
        ))
        .expect("create index");
        let shard = store
            .resolve_shard(graph_id, gleaph_graph_kernel::federation::ShardId::new(0))
            .expect("shard");
        assert_eq!(shard.index_canister, candid::Principal::anonymous());
        assert_eq!(
            store.graph_index_cluster(graph_id).expect("cluster"),
            Vec::<candid::Principal>::new()
        );
    }

    #[test]
    fn unassigned_index_groups_reports_indexless_groups() {
        let store = RouterStore::new();
        let graph_name = "tenant.indexless.groups";
        let graph_id = register_test_graph(&store, graph_name);
        let admin = candid::Principal::from_slice(&[1; 29]);
        // index_group_size defaults to 1, so shard 0 → group 0 and shard 1 → group 1.
        register_indexless_shard(&store, admin, graph_name, 0, 0x50);
        register_indexless_shard(&store, admin, graph_name, 1, 0x51);
        assert_eq!(
            store.unassigned_index_groups(graph_id).expect("groups"),
            vec![0, 1]
        );
    }

    #[test]
    fn attach_provisioned_index_canisters_assigns_cluster_and_attaches() {
        let store = RouterStore::new();
        let graph_name = "tenant.indexless.attach";
        let graph_id = register_test_graph(&store, graph_name);
        let admin = candid::Principal::from_slice(&[1; 29]);
        register_indexless_shard(&store, admin, graph_name, 0, 0x50);

        let canister = candid::Principal::from_slice(&[0x60; 29]);
        futures::executor::block_on(store.attach_provisioned_index_canisters(
            graph_id,
            &std::collections::BTreeMap::from([(0u32, canister)]),
        ))
        .expect("attach");
        let shard = store
            .resolve_shard(graph_id, gleaph_graph_kernel::federation::ShardId::new(0))
            .expect("shard");
        assert_eq!(shard.index_canister, canister);
        assert_eq!(
            store.graph_index_cluster(graph_id).expect("cluster"),
            vec![canister]
        );
        // Idempotent: re-attaching the same canister is a no-op.
        futures::executor::block_on(store.attach_provisioned_index_canisters(
            graph_id,
            &std::collections::BTreeMap::from([(0u32, canister)]),
        ))
        .expect("re-attach");
        assert_eq!(
            store
                .resolve_shard(graph_id, gleaph_graph_kernel::federation::ShardId::new(0))
                .expect("shard")
                .index_canister,
            canister
        );
    }
}
