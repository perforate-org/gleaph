//! Router-owned physical index catalog (ADR 0009, ADR 0059).
//!
//! `ROUTER_NAMED_INDEXES` is the sole source of truth. Planner membership is derived from its
//! `Active` records; lifecycle records in every other state remain invisible to queries.

use std::borrow::Cow;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::index::{
    EdgeIndexDirection, IndexMaintenancePhase, IndexedEdgeMembership, IndexedPropertyCatalog,
    IndexedPropertyKind, IndexedVertexMembership, PhysicalIndexId,
};
use gleaph_migration_api::{
    MigrationFailureCode, ResolvedSchemaMigrationGraph, SchemaMigrationGraphSelector,
};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use ic_stable_structures::{Cell, Memory};
use serde::Deserialize;

use crate::facade::stable::graph_catalog;
use crate::facade::stable::{
    ROUTER_INDEX_CATALOG_EPOCH, ROUTER_NAMED_INDEXES, ROUTER_NEXT_PHYSICAL_INDEX_ID,
};
use crate::facade::store::RouterStore;
use crate::planner_stats::{EdgeIndexMembership, RouterGraphStats, VertexIndexMembership};
use crate::state::RouterError;

const MAX_MIGRATION_ID_BYTES: usize = 256;
const MAX_TARGET_SHARDS: usize = 4_096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NamedIndexKey {
    pub graph_id: GraphId,
    pub index_name_id: IndexNameId,
}

impl NamedIndexKey {
    pub const fn new(graph_id: GraphId, index_name_id: IndexNameId) -> Self {
        Self {
            graph_id,
            index_name_id,
        }
    }
}

/// Immutable metadata captured before a physical index build begins.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) enum IndexBuildMetadata {
    /// Temporary ADR 0009 bridge for non-migration `CREATE INDEX`: the catalog row is published
    /// directly as `Active`. Migration callers must use the durable variant below.
    ImmediateActive,
    Migration {
        migration_id: String,
        prepared_catalog_epoch: u64,
        topology_epoch: u64,
        prepared_at_ns: u64,
    },
}

/// Per-shard seal proof returned by the graph-index owner.
#[derive(CandidType, Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct IndexShardWatermark {
    pub shard_id: u32,
    pub admitted_through: u64,
    pub drained_through: u64,
}

/// Durable progress for one graph-index canister target.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) enum IndexLifecycleTargetState {
    Registering,
    Registered,
    Building {
        seeded_items: u64,
    },
    Built {
        seeded_items: u64,
    },
    Sealing,
    Converged {
        watermarks: Vec<IndexShardWatermark>,
    },
    Cleaning,
    Cleaned,
}

/// Immutable target identity plus its resumable owner-local progress.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct IndexLifecycleTarget {
    pub index_canister: Principal,
    /// Sorted unique shard ids routed to this graph-index target.
    pub shard_ids: Vec<u32>,
    pub state: IndexLifecycleTargetState,
}

/// Durable physical-index lifecycle. Only `Active` is planner-visible.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) enum IndexLifecycleState {
    Preparing {
        targets: Vec<IndexLifecycleTarget>,
    },
    Building {
        targets: Vec<IndexLifecycleTarget>,
    },
    Sealing {
        catalog_epoch: u64,
        targets: Vec<IndexLifecycleTarget>,
        started_at_ns: u64,
    },
    Active {
        catalog_epoch: u64,
        activated_at_ns: u64,
    },
    Aborting {
        catalog_epoch: u64,
        failure: MigrationFailureCode,
        targets: Vec<IndexLifecycleTarget>,
        started_at_ns: u64,
    },
}

impl IndexLifecycleState {
    pub(crate) const fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

/// Canonical Router record for one logical index name and one physical posting namespace.
#[derive(CandidType, Clone, Debug, Deserialize, Eq, PartialEq)]
pub(crate) struct IndexDefRecord {
    pub physical_index_id: PhysicalIndexId,
    pub graph_selector: SchemaMigrationGraphSelector,
    pub resolved_graph: ResolvedSchemaMigrationGraph,
    pub kind: IndexedPropertyKind,
    pub property_id: PropertyId,
    pub label_id: u16,
    /// Edge index direction (ADR 0012); `None` for vertex indexes.
    pub edge_direction: Option<EdgeIndexDirection>,
    pub build: IndexBuildMetadata,
    pub lifecycle: IndexLifecycleState,
}

/// Inputs already resolved by migration preflight. This API persists the immutable build snapshot;
/// orchestration and export are intentionally owned by later ADR 0059 slices.
#[allow(
    dead_code,
    reason = "called by the ADR 0059 migration orchestration slice"
)]
pub(crate) struct PrepareIndexLifecycleArgs {
    pub graph_selector: SchemaMigrationGraphSelector,
    pub resolved_graph: ResolvedSchemaMigrationGraph,
    pub kind: IndexedPropertyKind,
    pub property_id: PropertyId,
    pub label_id: u16,
    pub edge_direction: Option<EdgeIndexDirection>,
    pub migration_id: String,
    pub topology_epoch: u64,
    pub targets: Vec<IndexLifecycleTarget>,
    pub prepared_at_ns: u64,
}

impl Storable for NamedIndexKey {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.index_name_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut index = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        index.copy_from_slice(&bytes[4..6]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            index_name_id: IndexNameId::from_le_bytes(index),
        }
    }
}

impl Storable for IndexDefRecord {
    // ADR 0059 is a pre-release persistence break: decode exactly the new record, without a
    // legacy branch or synthetic defaults.
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("IndexDefRecord candid encoding must succeed"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("IndexDefRecord candid encoding must succeed")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("invalid ADR 0059 IndexDefRecord encoding")
    }
}

/// Resolve the one active vertex posting namespace for a logical property scope.
///
/// A concrete label is preferred when the query proves one. An omitted label is accepted only
/// when the graph has exactly one active namespace for the property; selecting an arbitrary label
/// would silently return false negatives. Preparing, Building, Sealing, and Aborting records are
/// never considered.
pub(crate) fn active_vertex_physical_index(
    graph_id: GraphId,
    label_id: Option<VertexLabelId>,
    property_id: PropertyId,
) -> Result<PhysicalIndexId, RouterError> {
    let label_raw = label_id.map(VertexLabelId::raw);
    let ids = active_physical_ids(graph_id, |def| {
        def.lifecycle.is_active()
            && def.kind == IndexedPropertyKind::Vertex
            && def.property_id == property_id
            && label_raw.is_none_or(|label| def.label_id == label)
    });
    unique_active_physical_index(ids, "vertex", property_id)
}

/// Resolve the one active edge posting namespace for a logical property scope.
///
/// `query_direction` is used to retain only indexes whose stored direction covers every storage
/// class requested by the query. A zero- or multi-match result fails closed.
pub(crate) fn active_edge_physical_index(
    graph_id: GraphId,
    label_id: Option<u16>,
    property_id: PropertyId,
    query_direction: Option<gleaph_gql::types::EdgeDirection>,
) -> Result<PhysicalIndexId, RouterError> {
    let ids = active_physical_ids(graph_id, |def| {
        if !def.lifecycle.is_active()
            || def.kind != IndexedPropertyKind::Edge
            || def.property_id != property_id
            || label_id.is_some_and(|label| def.label_id != label)
        {
            return false;
        }
        match (def.edge_direction, query_direction) {
            (Some(index_direction), Some(query_direction)) => {
                crate::edge_index_direction::index_applies_to_query(
                    index_direction,
                    query_direction,
                )
            }
            (Some(_), None) => true,
            (None, _) => false,
        }
    });
    unique_active_physical_index(ids, "edge", property_id)
}

fn active_physical_ids(
    graph_id: GraphId,
    predicate: impl Fn(&IndexDefRecord) -> bool,
) -> Vec<PhysicalIndexId> {
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        map.range((Bound::Included(start), named_index_graph_upper(graph_id)))
            .filter_map(|entry| {
                predicate(&entry.value()).then_some(entry.value().physical_index_id)
            })
            .collect()
    })
}

fn unique_active_physical_index(
    mut ids: Vec<PhysicalIndexId>,
    kind: &str,
    property_id: PropertyId,
) -> Result<PhysicalIndexId, RouterError> {
    // Count matching catalog rows, not just distinct ids. A duplicate physical id is catalog
    // corruption and must fail closed rather than being silently normalized away.
    ids.sort_unstable();
    match ids.as_slice() {
        [id] => Ok(*id),
        [] => Err(RouterError::InvalidArgument(format!(
            "no active {kind} property index namespace for property {}",
            property_id.raw()
        ))),
        _ => Err(RouterError::InvalidArgument(format!(
            "multiple active {kind} property index namespaces for property {}",
            property_id.raw()
        ))),
    }
}

/// Reverse-resolves one vertex index definition's dotted leaf identity.
///
/// The projection is infallible and fail-closed: a valid flat definition projects
/// `Some(("", 0))`, a valid nested definition projects its canonical dotted leaf path plus the
/// interned top-level property that stores the root record, and an inconsistent row — missing
/// reverse name, malformed dotted path, or missing/zero ancestor identity — projects `None` so
/// callers omit the whole membership instead of flattening it to a flat shape. Graph consumes
/// the ancestor id because it owns no property-name catalog; omitting an inconsistent row keeps
/// the canonical scan fallback available.
pub(crate) fn vertex_leaf_projection(
    graph_id: GraphId,
    leaf_property_id: PropertyId,
) -> Option<(String, u32)> {
    let field_path = RouterStore::new()
        .reverse_property_name(graph_id, leaf_property_id)
        .ok()?;
    if !field_path.contains('.') {
        return Some((String::new(), 0));
    }
    if field_path.split('.').any(|component| component.is_empty()) {
        return None;
    }
    let top = field_path.split('.').next().unwrap_or_default();
    let ancestor_property_id = RouterStore::new()
        .lookup_property_id(graph_id, top)
        .map(|id| id.raw())
        .unwrap_or(0);
    (ancestor_property_id != 0).then_some((field_path, ancestor_property_id))
}

pub(crate) fn load_graph_stats(graph_id: GraphId) -> RouterGraphStats {
    let mut vertex = std::collections::BTreeSet::new();
    let mut edge = std::collections::BTreeSet::new();
    let mut vertex_indexes = std::collections::BTreeSet::new();
    let mut edge_indexes = std::collections::BTreeSet::new();

    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        for entry in map.range((Bound::Included(start), named_index_graph_upper(graph_id))) {
            let def = entry.value();
            let IndexLifecycleState::Active { catalog_epoch, .. } = def.lifecycle else {
                continue;
            };
            match def.kind {
                IndexedPropertyKind::Vertex => {
                    vertex.insert(def.property_id);
                    let Some((field_path, ancestor_property_id)) =
                        vertex_leaf_projection(graph_id, def.property_id)
                    else {
                        // An inconsistent catalog row is omitted whole, never flattened to a
                        // flat membership; the canonical scan fallback remains available.
                        continue;
                    };
                    vertex_indexes.insert(VertexIndexMembership {
                        physical_index_id: def.physical_index_id,
                        catalog_epoch,
                        property_id: def.property_id,
                        label_id: def.label_id,
                        field_path,
                        ancestor_property_id,
                    });
                }
                IndexedPropertyKind::Edge => {
                    edge.insert(def.property_id);
                    let Some(direction) = def.edge_direction else {
                        continue;
                    };
                    edge_indexes.insert(EdgeIndexMembership {
                        physical_index_id: def.physical_index_id,
                        catalog_epoch,
                        property_id: def.property_id,
                        label_id: def.label_id,
                        direction,
                        field_path: RouterStore::new()
                            .reverse_property_name(graph_id, def.property_id)
                            .ok()
                            .filter(|name| name.contains('.'))
                            .unwrap_or_default(),
                    });
                }
            }
        }
    });

    RouterGraphStats::from_catalog_with_indexes(
        graph_id,
        vertex,
        edge,
        vertex_indexes,
        edge_indexes,
    )
}

/// Project every lifecycle membership that can participate in index maintenance.
///
/// Preparing and Aborting records are intentionally excluded: neither owns a usable posting
/// namespace. Building uses the immutable prepare epoch captured in the migration metadata;
/// Sealing and Active use the epoch carried by their lifecycle state. The physical namespace is
/// always copied from the Router-owned record and is never derived from a logical property id.
pub(crate) fn load_indexed_property_catalog(graph_id: GraphId) -> IndexedPropertyCatalog {
    let mut vertex_indexes = Vec::new();
    let mut edge_indexes = Vec::new();

    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        for entry in map.range((Bound::Included(start), named_index_graph_upper(graph_id))) {
            let def = entry.value();
            let Some((catalog_epoch, phase)) = maintenance_phase(&def) else {
                continue;
            };
            match def.kind {
                IndexedPropertyKind::Vertex => {
                    let Some((field_path, ancestor_property_id)) =
                        vertex_leaf_projection(graph_id, def.property_id)
                    else {
                        // An inconsistent catalog row is omitted whole, never flattened to a
                        // flat membership; the canonical scan fallback remains available.
                        continue;
                    };
                    vertex_indexes.push(IndexedVertexMembership {
                        physical_index_id: def.physical_index_id,
                        catalog_epoch,
                        phase,
                        property_id: def.property_id.raw(),
                        label_id: def.label_id,
                        field_path,
                        ancestor_property_id,
                    });
                }
                IndexedPropertyKind::Edge => {
                    let Some(direction) = def.edge_direction else {
                        continue;
                    };
                    edge_indexes.push(IndexedEdgeMembership {
                        physical_index_id: def.physical_index_id,
                        catalog_epoch,
                        phase,
                        label_id: def.label_id,
                        property_id: def.property_id.raw(),
                        direction,
                        field_path: RouterStore::new()
                            .reverse_property_name(graph_id, def.property_id)
                            .ok()
                            .filter(|name| name.contains('.'))
                            .unwrap_or_default(),
                    });
                }
            }
        }
    });

    vertex_indexes.sort_by_key(|membership| {
        (
            membership.physical_index_id,
            membership.catalog_epoch,
            membership.property_id,
            membership.label_id,
            membership.field_path.clone(),
        )
    });
    edge_indexes.sort_by_key(|membership| {
        (
            membership.physical_index_id,
            membership.catalog_epoch,
            membership.property_id,
            membership.label_id,
            membership.direction,
            membership.field_path.clone(),
        )
    });
    IndexedPropertyCatalog {
        vertex_indexes,
        edge_indexes,
    }
}

/// Build-only catalog projection used by explicit backfill APIs. Unlike the maintenance
/// projection above, this intentionally exposes only Active namespaces.
pub(crate) fn load_active_indexed_property_catalog(graph_id: GraphId) -> IndexedPropertyCatalog {
    load_graph_stats(graph_id).to_active_indexed_property_catalog()
}

/// Subset the active indexed-property catalog to one ordered vertex batch's written properties
/// (ADR 0060 chunk-scoped postings). The graph consumes vertex memberships by `property_id`
/// only, so every membership for a written property is retained; memberships for properties the
/// batch does not write are dropped so the per-chunk wire stays proportional to the batch.
pub(crate) fn vertex_batch_indexed_property_catalog(
    graph_id: GraphId,
    property_ids: &std::collections::BTreeSet<u32>,
) -> IndexedPropertyCatalog {
    let mut catalog = load_active_indexed_property_catalog(graph_id);
    catalog
        .vertex_indexes
        .retain(|membership| property_ids.contains(&membership.property_id));
    catalog.edge_indexes.clear();
    catalog
}

/// Subset the active indexed-property catalog to one ordered vertex batch request's written
/// properties.
pub(crate) fn ordered_vertex_batch_catalog(
    request: &gleaph_graph_kernel::plan_exec::OrderedVertexBatchGraphRequestV1,
) -> IndexedPropertyCatalog {
    let property_ids = request
        .items
        .iter()
        .flat_map(|item| {
            item.resolved_initial_properties
                .iter()
                .map(|property| property.property_id.raw())
        })
        .collect::<std::collections::BTreeSet<_>>();
    vertex_batch_indexed_property_catalog(request.graph_id, &property_ids)
}

/// Subset the active indexed-property catalog to one ordered edge batch's written
/// `(label_id, property_id)` scopes (ADR 0060 chunk-scoped postings). The graph consumes edge
/// memberships by `(label_id, property_id)`, so memberships outside the batch's scopes are
/// dropped. Edges without a label can never carry an indexed edge property.
pub(crate) fn edge_batch_indexed_property_catalog(
    graph_id: GraphId,
    scopes: &std::collections::BTreeSet<(u16, u32)>,
) -> IndexedPropertyCatalog {
    let mut catalog = load_active_indexed_property_catalog(graph_id);
    catalog.vertex_indexes.clear();
    catalog
        .edge_indexes
        .retain(|membership| scopes.contains(&(membership.label_id, membership.property_id)));
    catalog
}

/// Subset the active indexed-property catalog to one ordered edge batch request's written
/// `(label_id, property_id)` scopes.
pub(crate) fn ordered_edge_batch_catalog(
    request: &gleaph_graph_kernel::plan_exec::OrderedEdgeBatchGraphRequestV1,
) -> IndexedPropertyCatalog {
    let scopes = request
        .items
        .iter()
        .filter_map(|item| {
            let label = item
                .catalog_edge_label_id
                .map(gleaph_graph_kernel::entry::EdgeLabelId::raw)?;
            Some(
                item.resolved_initial_edge_properties
                    .iter()
                    .map(move |property| (label, property.property_id.raw())),
            )
        })
        .flatten()
        .collect::<std::collections::BTreeSet<_>>();
    edge_batch_indexed_property_catalog(request.graph_id, &scopes)
}

/// Subset the active indexed-property catalog to one ordered mixed batch request's written vertex
/// properties and edge `(label_id, property_id)` scopes.
pub(crate) fn ordered_mixed_batch_catalog(
    request: &gleaph_graph_kernel::plan_exec::OrderedMixedBatchGraphRequestV1,
) -> IndexedPropertyCatalog {
    let mut catalog = load_active_indexed_property_catalog(request.graph_id);
    let mut vertex_properties = std::collections::BTreeSet::new();
    let mut edge_scopes = std::collections::BTreeSet::new();
    for operation in &request.operations {
        match operation {
            gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Vertex(item) => {
                for property in &item.resolved_initial_properties {
                    vertex_properties.insert(property.property_id.raw());
                }
            }
            gleaph_graph_kernel::plan_exec::OrderedMixedGraphOperationV1::Edge(item) => {
                let Some(label) = item
                    .catalog_edge_label_id
                    .map(gleaph_graph_kernel::entry::EdgeLabelId::raw)
                else {
                    continue;
                };
                for property in &item.resolved_initial_edge_properties {
                    edge_scopes.insert((label, property.property_id.raw()));
                }
            }
        }
    }
    catalog
        .vertex_indexes
        .retain(|membership| vertex_properties.contains(&membership.property_id));
    catalog
        .edge_indexes
        .retain(|membership| edge_scopes.contains(&(membership.label_id, membership.property_id)));
    catalog
}

fn maintenance_phase(record: &IndexDefRecord) -> Option<(u64, IndexMaintenancePhase)> {
    match record.lifecycle {
        IndexLifecycleState::Preparing { .. } | IndexLifecycleState::Aborting { .. } => None,
        IndexLifecycleState::Building { .. } => prepared_catalog_epoch(record)
            .map(|catalog_epoch| (catalog_epoch, IndexMaintenancePhase::Building)),
        IndexLifecycleState::Sealing { catalog_epoch, .. } => {
            Some((catalog_epoch, IndexMaintenancePhase::Sealing))
        }
        IndexLifecycleState::Active { catalog_epoch, .. } => {
            Some((catalog_epoch, IndexMaintenancePhase::Active))
        }
    }
}

/// ADR 0009 compatibility path. It remains direct-Active until migration lifecycle integration,
/// but still receives a unique monotonic physical namespace and a fully resolved graph identity.
pub(crate) fn create_named_index(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    entry: crate::planner_stats::IndexCatalogEntry,
    property_id: PropertyId,
    label_id: u16,
    edge_direction: Option<EdgeIndexDirection>,
    if_not_exists: bool,
) -> Result<(bool, bool), RouterError> {
    let named_key = NamedIndexKey::new(graph_id, index_name_id);
    if ROUTER_NAMED_INDEXES.with_borrow(|map| map.contains_key(&named_key)) {
        if if_not_exists {
            return Ok((false, false));
        }
        return Err(RouterError::Conflict(format!(
            "index already exists: {index_name_id}"
        )));
    }
    validate_kind_direction(entry.kind, edge_direction)?;
    reject_duplicate_edge_identity(graph_id, entry.kind, label_id, property_id, edge_direction)?;
    let graph_name = graph_catalog::graph_name(graph_id).ok_or_else(|| {
        RouterError::NotFound(format!("canonical graph name for graph {}", graph_id.raw()))
    })?;
    let property_was_active = is_property_registered(graph_id, entry.kind, property_id);
    let catalog_epoch = next_catalog_epoch()?;
    let physical_index_id = allocate_physical_index_id()?;
    ROUTER_INDEX_CATALOG_EPOCH.with_borrow_mut(|epoch| epoch.set(catalog_epoch));
    let def = IndexDefRecord {
        physical_index_id,
        graph_selector: SchemaMigrationGraphSelector::Named(graph_name.clone()),
        resolved_graph: ResolvedSchemaMigrationGraph {
            graph_id,
            graph_name,
        },
        kind: entry.kind,
        property_id,
        label_id,
        edge_direction,
        build: IndexBuildMetadata::ImmediateActive,
        lifecycle: IndexLifecycleState::Active {
            catalog_epoch,
            activated_at_ns: 0,
        },
    };
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        map.insert(named_key, def);
    });
    Ok((true, !property_was_active))
}

/// Inserts a hidden `Preparing` record after all immutable inputs have been validated. Allocation
/// occurs only after conflict checks, so deterministic rejections do not consume an id.
#[allow(
    dead_code,
    reason = "called by the ADR 0059 migration orchestration slice"
)]
pub(crate) fn prepare_named_index(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    args: PrepareIndexLifecycleArgs,
) -> Result<IndexDefRecord, RouterError> {
    preflight_prepare_index_scope(graph_id, &args)?;
    let named_key = NamedIndexKey::new(graph_id, index_name_id);
    if ROUTER_NAMED_INDEXES.with_borrow(|map| map.contains_key(&named_key)) {
        return Err(RouterError::Conflict(format!(
            "index already exists: {index_name_id}"
        )));
    }

    let prepared_catalog_epoch = next_catalog_epoch()?;
    let record = IndexDefRecord {
        physical_index_id: allocate_physical_index_id()?,
        graph_selector: args.graph_selector,
        resolved_graph: args.resolved_graph,
        kind: args.kind,
        property_id: args.property_id,
        label_id: args.label_id,
        edge_direction: args.edge_direction,
        build: IndexBuildMetadata::Migration {
            migration_id: args.migration_id,
            prepared_catalog_epoch,
            topology_epoch: args.topology_epoch,
            prepared_at_ns: args.prepared_at_ns,
        },
        lifecycle: IndexLifecycleState::Preparing {
            targets: args.targets,
        },
    };
    ROUTER_INDEX_CATALOG_EPOCH.with_borrow_mut(|epoch| epoch.set(prepared_catalog_epoch));
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        map.insert(named_key, record.clone());
    });
    Ok(record)
}

/// Validate every recoverable prepare-scope condition without mutating stable state.
///
/// The migration orchestration layer calls this before it interns the logical index name. The
/// named-key conflict is intentionally checked by that caller because this scope API has no name
/// argument; [`prepare_named_index`] repeats the same scope validation immediately before its
/// first stable write.
pub(crate) fn preflight_prepare_index_scope(
    graph_id: GraphId,
    args: &PrepareIndexLifecycleArgs,
) -> Result<(), RouterError> {
    validate_prepare_args(graph_id, args)?;
    reject_duplicate_edge_identity(
        graph_id,
        args.kind,
        args.label_id,
        args.property_id,
        args.edge_direction,
    )?;
    if pending_index_migration_exists_for_other_migration(&args.migration_id) {
        return Err(RouterError::Conflict(
            "another physical index migration is still pending".into(),
        ));
    }
    next_catalog_epoch()?;
    ROUTER_NEXT_PHYSICAL_INDEX_ID.with_borrow(|next_id| {
        let raw = *next_id.get();
        if PhysicalIndexId::new(raw).is_none() {
            return Err(RouterError::Internal(
                "physical index allocator stored zero".into(),
            ));
        }
        if raw.checked_add(1).is_none() {
            return Err(RouterError::IdExhausted("physical index id".into()));
        }
        Ok(())
    })
}

/// Advances one lifecycle record after validating the durable state-machine edge. The graph
/// selector, resolved graph, topology snapshot, epochs captured at prepare, and physical id are
/// never rewritten by this operation.
#[allow(
    dead_code,
    reason = "called by the ADR 0059 migration orchestration slice"
)]
pub(crate) fn transition_index_lifecycle(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    next: IndexLifecycleState,
) -> Result<IndexDefRecord, RouterError> {
    validate_lifecycle_payload(&next)?;
    let key = NamedIndexKey::new(graph_id, index_name_id);
    let mut record = ROUTER_NAMED_INDEXES
        .with_borrow(|map| map.get(&key))
        .ok_or_else(|| RouterError::NotFound(index_name_id.to_string()))?;
    if record.lifecycle == next {
        return Ok(record);
    }
    validate_lifecycle_transition(&record, &next)?;
    record.lifecycle = next;
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        map.insert(key, record.clone());
    });
    Ok(record)
}

pub(crate) fn get_named_index(
    graph_id: GraphId,
    index_name_id: IndexNameId,
) -> Option<IndexDefRecord> {
    ROUTER_NAMED_INDEXES.with_borrow(|map| map.get(&NamedIndexKey::new(graph_id, index_name_id)))
}

/// Atomically advances the global catalog fence and moves a fully built namespace to Sealing.
pub(crate) fn begin_index_sealing(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    targets: Vec<IndexLifecycleTarget>,
    started_at_ns: u64,
) -> Result<IndexDefRecord, RouterError> {
    let key = NamedIndexKey::new(graph_id, index_name_id);
    let mut record = ROUTER_NAMED_INDEXES
        .with_borrow(|map| map.get(&key))
        .ok_or_else(|| RouterError::NotFound(index_name_id.to_string()))?;
    let prepared = prepared_catalog_epoch(&record).ok_or_else(|| {
        RouterError::InvalidState("immediate-active index cannot enter Sealing".into())
    })?;
    let current = current_index_catalog_epoch();
    if current < prepared {
        return Err(RouterError::Conflict(format!(
            "index catalog epoch regressed from prepared {prepared} to {current}"
        )));
    }
    // Sequential sub-builds of one migration prepare together and seal one after another, so a
    // later sub-build legitimately sees a global epoch ahead of its own prepared epoch; the seal
    // fence below is always derived from the newest epoch.
    let next_epoch = current
        .checked_add(1)
        .ok_or_else(|| RouterError::IdExhausted("index catalog epoch".into()))?;
    let next = IndexLifecycleState::Sealing {
        catalog_epoch: next_epoch,
        targets,
        started_at_ns,
    };
    validate_lifecycle_transition(&record, &next)?;
    record.lifecycle = next;
    ROUTER_INDEX_CATALOG_EPOCH.with_borrow_mut(|epoch| epoch.set(next_epoch));
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        map.insert(key, record.clone());
    });
    Ok(record)
}

pub(crate) fn drop_named_index(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    if_exists: bool,
) -> Result<Option<IndexDefRecord>, RouterError> {
    let exists = ROUTER_NAMED_INDEXES
        .with_borrow(|map| map.contains_key(&NamedIndexKey::new(graph_id, index_name_id)));
    if !exists {
        return if if_exists {
            Ok(None)
        } else {
            Err(RouterError::NotFound(index_name_id.to_string()))
        };
    }
    let next_epoch = next_catalog_epoch()?;
    let removed = ROUTER_NAMED_INDEXES
        .with_borrow_mut(|map| map.remove(&NamedIndexKey::new(graph_id, index_name_id)));
    ROUTER_INDEX_CATALOG_EPOCH.with_borrow_mut(|epoch| epoch.set(next_epoch));
    Ok(removed)
}

/// Active planner membership derived from the canonical lifecycle rows.
pub(crate) fn is_property_registered(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: PropertyId,
) -> bool {
    with_graph_records(graph_id, |def| {
        def.lifecycle.is_active() && def.kind == kind && def.property_id == property_id
    })
}

pub(crate) fn edge_index_uses_property_label(
    graph_id: GraphId,
    property_id: PropertyId,
    label_id: u16,
) -> bool {
    with_graph_records(graph_id, |def| {
        def.lifecycle.is_active()
            && def.kind == IndexedPropertyKind::Edge
            && def.property_id == property_id
            && def.label_id == label_id
    })
}

pub(crate) fn purge_graph_indexes(graph_id: GraphId) {
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        let keys: Vec<_> = map
            .range((Bound::Included(start), named_index_graph_upper(graph_id)))
            .map(|entry| *entry.key())
            .collect();
        for key in keys {
            map.remove(&key);
        }
    });
    // The physical id allocator is deliberately not rewound: ids are never reused.
}

fn allocate_physical_index_id() -> Result<PhysicalIndexId, RouterError> {
    ROUTER_NEXT_PHYSICAL_INDEX_ID.with_borrow_mut(allocate_from)
}

pub(crate) fn current_index_catalog_epoch() -> u64 {
    ROUTER_INDEX_CATALOG_EPOCH.with_borrow(|epoch| *epoch.get())
}

fn next_catalog_epoch() -> Result<u64, RouterError> {
    current_index_catalog_epoch()
        .checked_add(1)
        .ok_or_else(|| RouterError::IdExhausted("index catalog epoch".into()))
}

fn allocate_from<M: Memory>(next_id: &mut Cell<u64, M>) -> Result<PhysicalIndexId, RouterError> {
    let raw = *next_id.get();
    let id = PhysicalIndexId::new(raw)
        .ok_or_else(|| RouterError::Internal("physical index allocator stored zero".into()))?;
    let next = raw
        .checked_add(1)
        .ok_or_else(|| RouterError::IdExhausted("physical index id".into()))?;
    next_id.set(next);
    Ok(id)
}

fn validate_prepare_args(
    graph_id: GraphId,
    args: &PrepareIndexLifecycleArgs,
) -> Result<(), RouterError> {
    validate_kind_direction(args.kind, args.edge_direction)?;
    if args.resolved_graph.graph_id != graph_id {
        return Err(RouterError::InvalidArgument(
            "resolved graph id does not match the index catalog key".into(),
        ));
    }
    let canonical_name = graph_catalog::graph_name(graph_id).ok_or_else(|| {
        RouterError::NotFound(format!("canonical graph name for graph {}", graph_id.raw()))
    })?;
    if args.resolved_graph.graph_name != canonical_name {
        return Err(RouterError::InvalidArgument(
            "resolved graph name is not canonical for its graph id".into(),
        ));
    }
    if let SchemaMigrationGraphSelector::Named(name) = &args.graph_selector
        && name != &canonical_name
    {
        return Err(RouterError::InvalidArgument(
            "named graph selector does not match the resolved graph".into(),
        ));
    }
    if args.migration_id.is_empty() || args.migration_id.len() > MAX_MIGRATION_ID_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "migration id must be 1..={MAX_MIGRATION_ID_BYTES} bytes"
        )));
    }
    let target_shard_count = args
        .targets
        .iter()
        .map(|target| target.shard_ids.len())
        .sum::<usize>();
    if args.targets.is_empty() || target_shard_count == 0 || target_shard_count > MAX_TARGET_SHARDS
    {
        return Err(RouterError::InvalidArgument(format!(
            "target shard snapshot must contain 1..={MAX_TARGET_SHARDS} shards"
        )));
    }
    if args
        .targets
        .windows(2)
        .any(|pair| pair[0].index_canister.as_slice() >= pair[1].index_canister.as_slice())
        || args.targets.iter().any(|target| {
            target.index_canister == Principal::anonymous()
                || !matches!(target.state, IndexLifecycleTargetState::Registering)
                || target.shard_ids.is_empty()
                || target.shard_ids.windows(2).any(|pair| pair[0] >= pair[1])
        })
    {
        return Err(RouterError::InvalidArgument(
            "index targets and each target's shards must be non-empty, sorted, and unique".into(),
        ));
    }
    let mut all_shards = args
        .targets
        .iter()
        .flat_map(|target| target.shard_ids.iter().copied())
        .collect::<Vec<_>>();
    all_shards.sort_unstable();
    if all_shards.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(RouterError::InvalidArgument(
            "a shard may belong to only one index target".into(),
        ));
    }
    Ok(())
}

fn validate_kind_direction(
    kind: IndexedPropertyKind,
    edge_direction: Option<EdgeIndexDirection>,
) -> Result<(), RouterError> {
    match (kind, edge_direction) {
        (IndexedPropertyKind::Vertex, None) | (IndexedPropertyKind::Edge, Some(_)) => Ok(()),
        (IndexedPropertyKind::Vertex, Some(_)) => Err(RouterError::InvalidArgument(
            "vertex index cannot carry an edge direction".into(),
        )),
        (IndexedPropertyKind::Edge, None) => Err(RouterError::InvalidArgument(
            "edge index requires an edge direction".into(),
        )),
    }
}

fn validate_lifecycle_payload(next: &IndexLifecycleState) -> Result<(), RouterError> {
    let targets = lifecycle_targets(next);
    if let Some(targets) = targets {
        if targets.is_empty() || targets.len() > MAX_TARGET_SHARDS {
            return Err(RouterError::InvalidArgument(
                "index lifecycle must retain its non-empty bounded target set".into(),
            ));
        }
        for target in targets {
            if let IndexLifecycleTargetState::Converged { watermarks } = &target.state
                && (watermarks.len() != target.shard_ids.len()
                    || watermarks
                        .iter()
                        .zip(&target.shard_ids)
                        .any(|(watermark, shard)| {
                            watermark.shard_id != *shard
                                || watermark.drained_through < watermark.admitted_through
                        }))
            {
                return Err(RouterError::InvalidArgument(
                    "converged target requires one drained watermark per captured shard".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_lifecycle_transition(
    record: &IndexDefRecord,
    next: &IndexLifecycleState,
) -> Result<(), RouterError> {
    let valid = match (&record.lifecycle, next) {
        (
            IndexLifecycleState::Preparing { targets: current },
            IndexLifecycleState::Preparing { targets: next },
        ) => valid_target_progress(current, next),
        (
            IndexLifecycleState::Preparing { targets: current },
            IndexLifecycleState::Building { targets: next },
        ) => {
            same_target_identity(current, next)
                && current
                    .iter()
                    .all(|target| matches!(target.state, IndexLifecycleTargetState::Registered))
                && next.iter().all(|target| {
                    matches!(
                        target.state,
                        IndexLifecycleTargetState::Building { seeded_items: 0 }
                    )
                })
        }
        (
            IndexLifecycleState::Building { targets: current },
            IndexLifecycleState::Building { targets: next },
        ) => valid_target_progress(current, next),
        (
            IndexLifecycleState::Building { targets: current },
            IndexLifecycleState::Sealing {
                catalog_epoch,
                targets: next,
                ..
            },
        ) => {
            prepared_catalog_epoch(record).is_some_and(|prepared| *catalog_epoch > prepared)
                && same_target_identity(current, next)
                && current
                    .iter()
                    .all(|target| matches!(target.state, IndexLifecycleTargetState::Built { .. }))
                && next
                    .iter()
                    .all(|target| matches!(target.state, IndexLifecycleTargetState::Sealing))
        }
        (
            IndexLifecycleState::Sealing {
                targets: current, ..
            },
            IndexLifecycleState::Sealing { targets: next, .. },
        ) => valid_target_progress(current, next),
        (
            IndexLifecycleState::Sealing {
                catalog_epoch: sealed,
                targets,
                ..
            },
            IndexLifecycleState::Active { catalog_epoch, .. },
        ) => {
            catalog_epoch == sealed
                && targets.iter().all(|target| {
                    matches!(target.state, IndexLifecycleTargetState::Converged { .. })
                })
        }
        (
            IndexLifecycleState::Preparing { .. }
            | IndexLifecycleState::Building { .. }
            | IndexLifecycleState::Sealing { .. },
            IndexLifecycleState::Aborting { .. },
        ) => true,
        (
            IndexLifecycleState::Aborting {
                catalog_epoch,
                failure,
                targets: current,
                ..
            },
            IndexLifecycleState::Aborting {
                catalog_epoch: next_epoch,
                failure: next_failure,
                targets: next,
                ..
            },
        ) => {
            catalog_epoch == next_epoch
                && failure == next_failure
                && valid_target_progress(current, next)
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(RouterError::Conflict(format!(
            "invalid index lifecycle transition from {:?} to {:?}",
            record.lifecycle, next
        )))
    }
}

fn lifecycle_targets(state: &IndexLifecycleState) -> Option<&[IndexLifecycleTarget]> {
    match state {
        IndexLifecycleState::Preparing { targets }
        | IndexLifecycleState::Building { targets }
        | IndexLifecycleState::Sealing { targets, .. }
        | IndexLifecycleState::Aborting { targets, .. } => Some(targets),
        IndexLifecycleState::Active { .. } => None,
    }
}

fn same_target_identity(current: &[IndexLifecycleTarget], next: &[IndexLifecycleTarget]) -> bool {
    current.len() == next.len()
        && current.iter().zip(next).all(|(current, next)| {
            current.index_canister == next.index_canister && current.shard_ids == next.shard_ids
        })
}

fn valid_target_progress(current: &[IndexLifecycleTarget], next: &[IndexLifecycleTarget]) -> bool {
    same_target_identity(current, next)
        && current
            .iter()
            .zip(next)
            .all(|(current, next)| valid_target_state_transition(&current.state, &next.state))
}

fn valid_target_state_transition(
    current: &IndexLifecycleTargetState,
    next: &IndexLifecycleTargetState,
) -> bool {
    if current == next {
        return true;
    }
    match (current, next) {
        (IndexLifecycleTargetState::Registering, IndexLifecycleTargetState::Registered) => true,
        (
            IndexLifecycleTargetState::Building {
                seeded_items: current,
            },
            IndexLifecycleTargetState::Building { seeded_items: next },
        ) => next >= current,
        (
            IndexLifecycleTargetState::Building {
                seeded_items: current,
            },
            IndexLifecycleTargetState::Built { seeded_items: next },
        ) => next >= current,
        (IndexLifecycleTargetState::Sealing, IndexLifecycleTargetState::Converged { .. }) => true,
        (IndexLifecycleTargetState::Cleaning, IndexLifecycleTargetState::Cleaned) => true,
        _ => false,
    }
}

fn prepared_catalog_epoch(record: &IndexDefRecord) -> Option<u64> {
    match &record.build {
        IndexBuildMetadata::Migration {
            prepared_catalog_epoch,
            ..
        } => Some(*prepared_catalog_epoch),
        IndexBuildMetadata::ImmediateActive => None,
    }
}

fn reject_duplicate_edge_identity(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    label_id: u16,
    property_id: PropertyId,
    edge_direction: Option<EdgeIndexDirection>,
) -> Result<(), RouterError> {
    if kind == IndexedPropertyKind::Edge
        && with_graph_records(graph_id, |def| {
            def.kind == IndexedPropertyKind::Edge
                && def.label_id == label_id
                && def.property_id == property_id
                && def.edge_direction == edge_direction
        })
    {
        return Err(RouterError::Conflict(format!(
            "edge index already exists for label {label_id}, property {property_id}, direction {edge_direction:?}"
        )));
    }
    Ok(())
}

fn pending_index_migration_exists_for_other_migration(migration_id: &str) -> bool {
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        map.iter().any(|entry| match &entry.value().build {
            // Lifecycle rows of the same migration are prepared together; only a non-Active row
            // owned by a different migration blocks this prepare (the migration ledger gate keeps
            // one pending index migration at a time).
            IndexBuildMetadata::Migration {
                migration_id: other,
                ..
            } if other != migration_id => !entry.value().lifecycle.is_active(),
            IndexBuildMetadata::Migration { .. } | IndexBuildMetadata::ImmediateActive => false,
        })
    })
}

fn with_graph_records(graph_id: GraphId, predicate: impl Fn(&IndexDefRecord) -> bool) -> bool {
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        map.range((Bound::Included(start), named_index_graph_upper(graph_id)))
            .any(|entry| predicate(&entry.value()))
    })
}

fn named_index_graph_upper(graph_id: GraphId) -> Bound<NamedIndexKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(NamedIndexKey::new(
            GraphId::from_raw(next),
            IndexNameId::from_raw(0),
        )),
        None => Bound::Unbounded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use gleaph_graph_kernel::index::EdgeIndexDirection;
    use ic_stable_structures::{BTreeMap, VectorMemory};

    fn register_graph(graph: GraphId, name: &str) {
        graph_catalog::insert_graph_name(name, graph).expect("register test graph name");
    }

    fn target(state: IndexLifecycleTargetState) -> Vec<IndexLifecycleTarget> {
        vec![IndexLifecycleTarget {
            index_canister: Principal::from_slice(&[8]),
            shard_ids: vec![1, 4],
            state,
        }]
    }

    fn migration_record(lifecycle: IndexLifecycleState) -> IndexDefRecord {
        IndexDefRecord {
            physical_index_id: PhysicalIndexId::new(41).expect("nonzero id"),
            graph_selector: SchemaMigrationGraphSelector::Named("catalog_graph".into()),
            resolved_graph: ResolvedSchemaMigrationGraph {
                graph_id: GraphId::from_raw(73),
                graph_name: "catalog_graph".into(),
            },
            kind: IndexedPropertyKind::Edge,
            property_id: PropertyId::from_raw(42),
            label_id: 3,
            edge_direction: Some(EdgeIndexDirection::Any),
            build: IndexBuildMetadata::Migration {
                migration_id: "000001_index".into(),
                prepared_catalog_epoch: 7,
                topology_epoch: 9,
                prepared_at_ns: 11,
            },
            lifecycle,
        }
    }

    fn prepare_args(graph: GraphId, graph_name: &str) -> PrepareIndexLifecycleArgs {
        PrepareIndexLifecycleArgs {
            graph_selector: SchemaMigrationGraphSelector::Named(graph_name.into()),
            resolved_graph: ResolvedSchemaMigrationGraph {
                graph_id: graph,
                graph_name: graph_name.into(),
            },
            kind: IndexedPropertyKind::Vertex,
            property_id: PropertyId::from_raw(7),
            label_id: 5,
            edge_direction: None,
            migration_id: "000001_index".into(),
            topology_epoch: 12,
            targets: target(IndexLifecycleTargetState::Registering),
            prepared_at_ns: 13,
        }
    }

    fn vertex_entry() -> crate::planner_stats::IndexCatalogEntry {
        crate::planner_stats::IndexCatalogEntry {
            kind: IndexedPropertyKind::Vertex,
            vertex_label: Some("Person".into()),
            edge_label: None,
            property: "age".into(),
            edge_direction: None,
        }
    }

    fn edge_entry() -> crate::planner_stats::IndexCatalogEntry {
        crate::planner_stats::IndexCatalogEntry {
            kind: IndexedPropertyKind::Edge,
            vertex_label: None,
            edge_label: Some("KNOWS".into()),
            property: "weight".into(),
            edge_direction: Some(gleaph_gql::types::EdgeDirection::AnyDirection),
        }
    }

    #[test]
    fn named_index_key_storable_roundtrip() {
        let key = NamedIndexKey::new(GraphId::from_raw(1), IndexNameId::from_raw(2));
        let decoded = NamedIndexKey::from_bytes(Cow::Owned(key.into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn lifecycle_record_codec_roundtrips_without_legacy_shape() {
        let states = [
            IndexLifecycleState::Preparing {
                targets: target(IndexLifecycleTargetState::Registering),
            },
            IndexLifecycleState::Building {
                targets: target(IndexLifecycleTargetState::Building { seeded_items: 17 }),
            },
            IndexLifecycleState::Sealing {
                catalog_epoch: 8,
                targets: target(IndexLifecycleTargetState::Sealing),
                started_at_ns: 29,
            },
            IndexLifecycleState::Active {
                catalog_epoch: 8,
                activated_at_ns: 31,
            },
            IndexLifecycleState::Aborting {
                catalog_epoch: 8,
                failure: MigrationFailureCode::TargetRejected,
                targets: target(IndexLifecycleTargetState::Cleaning),
                started_at_ns: 37,
            },
        ];
        for state in states {
            let record = migration_record(state);
            let decoded = IndexDefRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
            assert_eq!(decoded, record);
        }
    }

    #[test]
    fn lifecycle_record_reopens_from_stable_map() {
        let memory = VectorMemory::default();
        let key = NamedIndexKey::new(GraphId::from_raw(73), IndexNameId::from_raw(9));
        let record = migration_record(IndexLifecycleState::Building {
            targets: target(IndexLifecycleTargetState::Building { seeded_items: 100 }),
        });
        let mut first = BTreeMap::init(memory.clone());
        first.insert(key, record.clone());
        drop(first);

        let reopened = BTreeMap::<NamedIndexKey, IndexDefRecord, _>::init(memory);
        assert_eq!(reopened.get(&key), Some(record));
    }

    #[test]
    fn physical_ids_are_monotonic_across_reopen_and_overflow_does_not_mutate() {
        let memory = VectorMemory::default();
        let mut first = Cell::init(memory.clone(), 1u64);
        assert_eq!(allocate_from(&mut first).expect("id 1").raw(), 1);
        assert_eq!(allocate_from(&mut first).expect("id 2").raw(), 2);
        drop(first);

        let mut reopened = Cell::init(memory, 999u64);
        assert_eq!(allocate_from(&mut reopened).expect("id 3").raw(), 3);
        reopened.set(u64::MAX);
        assert!(matches!(
            allocate_from(&mut reopened),
            Err(RouterError::IdExhausted(_))
        ));
        assert_eq!(*reopened.get(), u64::MAX);
        reopened.set(0);
        assert!(matches!(
            allocate_from(&mut reopened),
            Err(RouterError::Internal(_))
        ));
        assert_eq!(*reopened.get(), 0);
    }

    #[test]
    fn planner_visibility_starts_only_after_valid_active_transition() {
        let graph = GraphId::from_raw(700_001);
        let graph_name = "lifecycle_active_gate";
        register_graph(graph, graph_name);
        let index_name = IndexNameId::from_raw(1);
        let prepared = prepare_named_index(graph, index_name, prepare_args(graph, graph_name))
            .expect("prepare index");
        assert!(!prepared.lifecycle.is_active());
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7)
        ));

        let invalid = transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Active {
                catalog_epoch: 11,
                activated_at_ns: 20,
            },
        );
        assert!(matches!(invalid, Err(RouterError::Conflict(_))));
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7)
        ));

        transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Preparing {
                targets: target(IndexLifecycleTargetState::Registered),
            },
        )
        .expect("register target");
        transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Building {
                targets: target(IndexLifecycleTargetState::Building { seeded_items: 0 }),
            },
        )
        .expect("start building");
        transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Building {
                targets: target(IndexLifecycleTargetState::Built { seeded_items: 4 }),
            },
        )
        .expect("finish building");
        transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Sealing {
                catalog_epoch: 11,
                targets: target(IndexLifecycleTargetState::Sealing),
                started_at_ns: 21,
            },
        )
        .expect("seal");
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7)
        ));
        transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Sealing {
                catalog_epoch: 11,
                targets: target(IndexLifecycleTargetState::Converged {
                    watermarks: vec![
                        IndexShardWatermark {
                            shard_id: 1,
                            admitted_through: 8,
                            drained_through: 8,
                        },
                        IndexShardWatermark {
                            shard_id: 4,
                            admitted_through: 8,
                            drained_through: 8,
                        },
                    ],
                }),
                started_at_ns: 21,
            },
        )
        .expect("converge targets");
        let active = transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Active {
                catalog_epoch: 11,
                activated_at_ns: 22,
            },
        )
        .expect("activate");
        assert_eq!(active.physical_index_id, prepared.physical_index_id);
        assert_eq!(active.resolved_graph, prepared.resolved_graph);
        assert!(is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7)
        ));
        assert_eq!(
            active_vertex_physical_index(
                graph,
                Some(VertexLabelId::from_raw(5)),
                PropertyId::from_raw(7)
            )
            .expect("active namespace")
            .raw(),
            active.physical_index_id.raw()
        );
    }

    #[test]
    fn aborting_is_hidden_and_cannot_be_reactivated() {
        let graph = GraphId::from_raw(700_002);
        let graph_name = "lifecycle_abort_gate";
        register_graph(graph, graph_name);
        let index_name = IndexNameId::from_raw(1);
        prepare_named_index(graph, index_name, prepare_args(graph, graph_name))
            .expect("prepare index");
        transition_index_lifecycle(
            graph,
            index_name,
            IndexLifecycleState::Aborting {
                catalog_epoch: 10,
                failure: MigrationFailureCode::TargetRejected,
                targets: target(IndexLifecycleTargetState::Cleaning),
                started_at_ns: 30,
            },
        )
        .expect("abort");
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7)
        ));
        assert!(matches!(
            transition_index_lifecycle(
                graph,
                index_name,
                IndexLifecycleState::Active {
                    catalog_epoch: 10,
                    activated_at_ns: 31,
                }
            ),
            Err(RouterError::Conflict(_))
        ));
        drop_named_index(graph, index_name, false).expect("remove aborting test record");
    }

    #[test]
    fn only_one_pending_migration_is_allowed_and_rejection_does_not_allocate() {
        let graph = GraphId::from_raw(700_005);
        let graph_name = "single_pending_index_migration";
        register_graph(graph, graph_name);
        let first_name = IndexNameId::from_raw(1);
        let second_name = IndexNameId::from_raw(2);
        let first = prepare_named_index(graph, first_name, prepare_args(graph, graph_name))
            .expect("prepare first index");

        let mut second_args = prepare_args(graph, graph_name);
        second_args.property_id = PropertyId::from_raw(8);
        // A different pending migration is rejected; same-migration rows are prepared together.
        second_args.migration_id = "000002_other_index".into();
        assert!(matches!(
            prepare_named_index(graph, second_name, second_args),
            Err(RouterError::Conflict(message)) if message.contains("still pending")
        ));

        drop_named_index(graph, first_name, false).expect("remove first pending record");
        let mut retry_args = prepare_args(graph, graph_name);
        retry_args.property_id = PropertyId::from_raw(8);
        retry_args.migration_id = "000002_other_index".into();
        let retry = prepare_named_index(graph, second_name, retry_args)
            .expect("prepare after pending record is removed");
        assert_eq!(
            retry.physical_index_id.raw(),
            first.physical_index_id.raw() + 1,
            "a deterministic pending conflict must not consume an id"
        );
        drop_named_index(graph, second_name, false).expect("remove pending test record");
    }

    #[test]
    fn vertex_drop_unregisters_property_only_when_last_active_index_is_gone() {
        let graph = GraphId::from_raw(700_003);
        register_graph(graph, "vertex_drop_membership");
        let property = PropertyId::from_raw(11);
        for (name, label) in [(1, 1), (2, 2)] {
            create_named_index(
                graph,
                IndexNameId::from_raw(name),
                vertex_entry(),
                property,
                label,
                None,
                false,
            )
            .expect("create vertex index");
        }
        drop_named_index(graph, IndexNameId::from_raw(1), false).expect("drop first index");
        assert!(is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            property
        ));
        drop_named_index(graph, IndexNameId::from_raw(2), false).expect("drop second index");
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            property
        ));
    }

    #[test]
    fn edge_drop_scopes_purge_to_property_label() {
        let graph = GraphId::from_raw(700_004);
        register_graph(graph, "edge_drop_membership");
        let property = PropertyId::from_raw(21);
        for (name, label) in [(1, 3), (2, 4)] {
            create_named_index(
                graph,
                IndexNameId::from_raw(name),
                edge_entry(),
                property,
                label,
                Some(EdgeIndexDirection::Outgoing),
                false,
            )
            .expect("create edge index");
        }
        let def = drop_named_index(graph, IndexNameId::from_raw(1), false)
            .expect("drop edge index")
            .expect("removed definition");
        assert_eq!(def.label_id, 3);
        assert!(!edge_index_uses_property_label(graph, property, 3));
        assert!(edge_index_uses_property_label(graph, property, 4));
    }

    #[test]
    fn range_scans_cover_the_max_graph_id() {
        let graph = GraphId::from_raw(u32::MAX);
        let vprop = PropertyId::from_raw(11);
        let eprop = PropertyId::from_raw(21);
        let resolved_graph = ResolvedSchemaMigrationGraph {
            graph_id: graph,
            graph_name: "max_graph_index_catalog".into(),
        };
        ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
            for (name, physical_id, kind, property_id, label_id, edge_direction) in [
                (1, 90, IndexedPropertyKind::Vertex, vprop, 1, None),
                (
                    2,
                    91,
                    IndexedPropertyKind::Edge,
                    eprop,
                    3,
                    Some(gleaph_graph_kernel::index::EdgeIndexDirection::Outgoing),
                ),
            ] {
                map.insert(
                    NamedIndexKey::new(graph, IndexNameId::from_raw(name)),
                    IndexDefRecord {
                        physical_index_id: PhysicalIndexId::new(physical_id)
                            .expect("nonzero physical id"),
                        graph_selector: SchemaMigrationGraphSelector::Named(
                            resolved_graph.graph_name.clone(),
                        ),
                        resolved_graph: resolved_graph.clone(),
                        kind,
                        property_id,
                        label_id,
                        edge_direction,
                        build: IndexBuildMetadata::ImmediateActive,
                        lifecycle: IndexLifecycleState::Active {
                            catalog_epoch: 0,
                            activated_at_ns: 0,
                        },
                    },
                );
            }
        });

        let stats = load_graph_stats(graph);
        assert!(stats.is_vertex_property_id_indexed(vprop));
        assert!(stats.is_edge_property_id_indexed(eprop));
        purge_graph_indexes(graph);
        let after = load_graph_stats(graph);
        assert!(!after.is_vertex_property_id_indexed(vprop));
        assert!(!after.is_edge_property_id_indexed(eprop));
    }

    #[test]
    fn maintenance_projection_preserves_namespace_phase_and_epoch() {
        let graph = GraphId::from_raw(700_006);
        let graph_name = "maintenance_projection";
        register_graph(graph, graph_name);
        let resolved_graph = ResolvedSchemaMigrationGraph {
            graph_id: graph,
            graph_name: graph_name.into(),
        };
        let migration_build = || IndexBuildMetadata::Migration {
            migration_id: "projection-test".into(),
            prepared_catalog_epoch: 7,
            topology_epoch: 1,
            prepared_at_ns: 1,
        };
        let make_record =
            |physical_index_id, index_name_id, kind, property_id, lifecycle, build| {
                ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
                    map.insert(
                        NamedIndexKey::new(graph, IndexNameId::from_raw(index_name_id)),
                        IndexDefRecord {
                            physical_index_id: PhysicalIndexId::new(physical_index_id)
                                .expect("physical id"),
                            graph_selector: SchemaMigrationGraphSelector::Named(graph_name.into()),
                            resolved_graph: resolved_graph.clone(),
                            kind,
                            property_id: PropertyId::from_raw(property_id),
                            label_id: 3,
                            edge_direction: (kind == IndexedPropertyKind::Edge)
                                .then_some(EdgeIndexDirection::Any),
                            build,
                            lifecycle,
                        },
                    );
                });
            };

        make_record(
            101,
            1,
            IndexedPropertyKind::Vertex,
            5,
            IndexLifecycleState::Active {
                catalog_epoch: 31,
                activated_at_ns: 0,
            },
            IndexBuildMetadata::ImmediateActive,
        );
        make_record(
            102,
            2,
            IndexedPropertyKind::Edge,
            6,
            IndexLifecycleState::Building {
                targets: target(IndexLifecycleTargetState::Building { seeded_items: 1 }),
            },
            migration_build(),
        );
        make_record(
            103,
            3,
            IndexedPropertyKind::Edge,
            7,
            IndexLifecycleState::Sealing {
                catalog_epoch: 8,
                targets: target(IndexLifecycleTargetState::Sealing),
                started_at_ns: 0,
            },
            migration_build(),
        );
        make_record(
            104,
            4,
            IndexedPropertyKind::Vertex,
            8,
            IndexLifecycleState::Preparing {
                targets: target(IndexLifecycleTargetState::Registering),
            },
            migration_build(),
        );
        make_record(
            105,
            5,
            IndexedPropertyKind::Vertex,
            9,
            IndexLifecycleState::Aborting {
                catalog_epoch: 9,
                failure: MigrationFailureCode::TargetRejected,
                targets: target(IndexLifecycleTargetState::Cleaning),
                started_at_ns: 0,
            },
            migration_build(),
        );

        let catalog = load_indexed_property_catalog(graph);
        assert_eq!(catalog.vertex_indexes.len(), 1);
        assert_eq!(catalog.vertex_indexes[0].physical_index_id.raw(), 101);
        assert_eq!(catalog.vertex_indexes[0].catalog_epoch, 31);
        assert_eq!(
            catalog.vertex_indexes[0].phase,
            IndexMaintenancePhase::Active
        );
        assert_eq!(catalog.edge_indexes.len(), 2);
        assert_eq!(catalog.edge_indexes[0].physical_index_id.raw(), 102);
        assert_eq!(catalog.edge_indexes[0].catalog_epoch, 7);
        assert_eq!(
            catalog.edge_indexes[0].phase,
            IndexMaintenancePhase::Building
        );
        assert_eq!(catalog.edge_indexes[1].physical_index_id.raw(), 103);
        assert_eq!(catalog.edge_indexes[1].catalog_epoch, 8);
        assert_eq!(
            catalog.edge_indexes[1].phase,
            IndexMaintenancePhase::Sealing
        );
        let active_only = load_active_indexed_property_catalog(graph);
        assert_eq!(active_only.vertex_indexes.len(), 1);
        assert_eq!(active_only.edge_indexes.len(), 0);
        assert!(
            load_graph_stats(graph)
                .to_active_indexed_property_catalog()
                .vertex_indexes
                .iter()
                .all(|membership| {
                    membership.physical_index_id.raw() == 101 && membership.catalog_epoch == 31
                })
        );
        purge_graph_indexes(graph);
    }

    #[test]
    fn prepare_scope_preflight_has_no_stable_side_effects() {
        let graph = GraphId::from_raw(700_007);
        let graph_name = "prepare_scope_preflight";
        register_graph(graph, graph_name);
        let args = prepare_args(graph, graph_name);
        let epoch_before = current_index_catalog_epoch();
        let next_physical_before = ROUTER_NEXT_PHYSICAL_INDEX_ID.with_borrow(|cell| *cell.get());
        let record_count_before = ROUTER_NAMED_INDEXES.with_borrow(|map| {
            map.iter()
                .filter(|entry| entry.key().graph_id == graph)
                .count()
        });

        preflight_prepare_index_scope(graph, &args).expect("valid scope preflight");

        assert_eq!(current_index_catalog_epoch(), epoch_before);
        assert_eq!(
            ROUTER_NEXT_PHYSICAL_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_physical_before
        );
        assert_eq!(
            ROUTER_NAMED_INDEXES.with_borrow(|map| {
                map.iter()
                    .filter(|entry| entry.key().graph_id == graph)
                    .count()
            }),
            record_count_before
        );

        let mut invalid = prepare_args(graph, graph_name);
        invalid.migration_id.clear();
        assert!(matches!(
            preflight_prepare_index_scope(graph, &invalid),
            Err(RouterError::InvalidArgument(_))
        ));
        assert_eq!(current_index_catalog_epoch(), epoch_before);
        assert_eq!(
            ROUTER_NEXT_PHYSICAL_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_physical_before
        );

        let mut invalid_direction = prepare_args(graph, graph_name);
        invalid_direction.kind = IndexedPropertyKind::Edge;
        assert!(matches!(
            preflight_prepare_index_scope(graph, &invalid_direction),
            Err(RouterError::InvalidArgument(message))
                if message.contains("requires an edge direction")
        ));
        assert_eq!(current_index_catalog_epoch(), epoch_before);
        assert_eq!(
            ROUTER_NEXT_PHYSICAL_INDEX_ID.with_borrow(|cell| *cell.get()),
            next_physical_before
        );
    }

    #[test]
    fn active_namespace_resolver_fails_closed_and_returns_exact_id() {
        let graph = GraphId::from_raw(700_008);
        let graph_name = "active_namespace_resolver";
        register_graph(graph, graph_name);
        let resolved_graph = ResolvedSchemaMigrationGraph {
            graph_id: graph,
            graph_name: graph_name.into(),
        };
        let insert_active = |name, physical_index_id, label_id| {
            ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
                map.insert(
                    NamedIndexKey::new(graph, IndexNameId::from_raw(name)),
                    IndexDefRecord {
                        physical_index_id: PhysicalIndexId::new(physical_index_id)
                            .expect("physical id"),
                        graph_selector: SchemaMigrationGraphSelector::Named(graph_name.into()),
                        resolved_graph: resolved_graph.clone(),
                        kind: IndexedPropertyKind::Vertex,
                        property_id: PropertyId::from_raw(12),
                        label_id,
                        edge_direction: None,
                        build: IndexBuildMetadata::ImmediateActive,
                        lifecycle: IndexLifecycleState::Active {
                            catalog_epoch: 1,
                            activated_at_ns: 0,
                        },
                    },
                );
            });
        };
        assert!(matches!(
            active_vertex_physical_index(graph, None, PropertyId::from_raw(12)),
            Err(RouterError::InvalidArgument(message)) if message.contains("no active")
        ));
        insert_active(1, 201, 3);
        assert_eq!(
            active_vertex_physical_index(graph, None, PropertyId::from_raw(12))
                .expect("one active namespace")
                .raw(),
            201
        );
        assert_eq!(
            active_vertex_physical_index(
                graph,
                Some(VertexLabelId::from_raw(3)),
                PropertyId::from_raw(12)
            )
            .expect("label-scoped namespace")
            .raw(),
            201
        );
        // A duplicate physical id is still two matching rows and must fail closed.
        insert_active(2, 201, 4);
        assert!(matches!(
            active_vertex_physical_index(graph, None, PropertyId::from_raw(12)),
            Err(RouterError::InvalidArgument(message)) if message.contains("multiple active")
        ));
        purge_graph_indexes(graph);
    }

    fn intern_property(graph: GraphId, name: &str) -> PropertyId {
        RouterStore::commit_intern_property_name(graph, name).expect("intern test property")
    }

    #[test]
    fn vertex_leaf_projection_projects_valid_flat_row() {
        let graph = GraphId::from_raw(700_101);
        let leaf = intern_property(graph, "age");
        assert_eq!(
            vertex_leaf_projection(graph, leaf),
            Some((String::new(), 0)),
            "a valid flat row projects the canonical explicit flat identity"
        );
    }

    #[test]
    fn vertex_leaf_projection_projects_valid_nested_row() {
        let graph = GraphId::from_raw(700_102);
        let ancestor = intern_property(graph, "stats");
        let leaf = intern_property(graph, "stats.score.meta");
        assert_eq!(
            vertex_leaf_projection(graph, leaf),
            Some(("stats.score.meta".to_owned(), ancestor.raw())),
            "a valid nested row projects its dotted path plus the interned ancestor id"
        );
    }

    #[test]
    fn vertex_leaf_projection_omits_missing_reverse_name() {
        let graph = GraphId::from_raw(700_103);
        let orphan_id = PropertyId::from_raw(9_999_003);
        assert_eq!(
            vertex_leaf_projection(graph, orphan_id),
            None,
            "a row whose reverse name is missing is omitted whole, never flattened"
        );
    }

    #[test]
    fn vertex_leaf_projection_omits_malformed_or_missing_path_component() {
        let graph = GraphId::from_raw(700_104);
        let trailing_dot = intern_property(graph, "stats.");
        let empty_component = intern_property(graph, "stats..score");
        assert_eq!(
            vertex_leaf_projection(graph, trailing_dot),
            None,
            "a dotted name with an empty tail component is omitted whole"
        );
        assert_eq!(
            vertex_leaf_projection(graph, empty_component),
            None,
            "a dotted name with an interior empty component is omitted whole"
        );
    }

    #[test]
    fn vertex_leaf_projection_omits_missing_or_zero_ancestor_identity() {
        let graph = GraphId::from_raw(700_105);
        let leaf = intern_property(graph, "orphan.score");
        assert_eq!(
            vertex_leaf_projection(graph, leaf),
            None,
            "a dotted row whose top-level property never resolved is omitted, not given ancestor 0"
        );
    }
}
