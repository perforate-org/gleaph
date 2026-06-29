//! Row-oriented index catalog in stable memory (ADR 0009 §4, ADR 0011 id keys).
//!
//! - `ROUTER_NAMED_INDEXES`: `(graph_id, index_name_id) → IndexDefRecord`
//! - `ROUTER_INDEXED_PROPERTY_SET`: `(graph_id, kind, property_id)` membership for planner + fan-out

use std::borrow::Cow;
use std::ops::Bound;

use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId, VertexLabelId};
use gleaph_graph_kernel::index::IndexedPropertyKind;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};

use crate::edge_index_direction::tag_from_byte;
use crate::facade::stable::{ROUTER_INDEXED_PROPERTY_SET, ROUTER_NAMED_INDEXES};
use crate::planner_stats::{EdgeIndexMembership, RouterGraphStats};
use crate::state::RouterError;

const KIND_VERTEX: u8 = 0;
const KIND_EDGE: u8 = 1;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IndexedPropertyKey {
    pub graph_id: GraphId,
    kind_tag: u8,
    pub property_id: PropertyId,
}

impl IndexedPropertyKey {
    pub const fn new(
        graph_id: GraphId,
        kind: IndexedPropertyKind,
        property_id: PropertyId,
    ) -> Self {
        Self {
            graph_id,
            kind_tag: kind_to_byte(kind),
            property_id,
        }
    }

    pub fn kind(&self) -> IndexedPropertyKind {
        kind_from_byte(self.kind_tag)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IndexDefRecord {
    pub kind: IndexedPropertyKind,
    pub property_id: PropertyId,
    pub label_id: u16,
    /// Edge index direction tag (ADR 0012); `0` for vertex indexes.
    pub edge_direction_tag: u8,
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

impl Storable for IndexedPropertyKey {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 9,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(9);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.push(self.kind_tag);
        out.extend_from_slice(&self.property_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        graph.copy_from_slice(&bytes[0..4]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            kind_tag: bytes[4],
            property_id: PropertyId::from_le_bytes(bytes[5..9].try_into().expect("property_id")),
        }
    }
}

impl Storable for IndexDefRecord {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.push(kind_to_byte(self.kind));
        out.extend_from_slice(&self.property_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out.push(self.edge_direction_tag);
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        Self {
            kind: kind_from_byte(bytes[0]),
            property_id: PropertyId::from_le_bytes(bytes[1..5].try_into().expect("property_id")),
            label_id: u16::from_le_bytes(bytes[5..7].try_into().expect("label_id")),
            edge_direction_tag: bytes.get(7).copied().unwrap_or(0),
        }
    }
}

/// Whether an active vertex property index exists for the exact `(graph_id, label_id, property_id)`
/// tuple. This is the source-of-truth coverage check used by both equality and numeric range
/// `SEARCH ... WHERE` predicates (ADR 0034 Slices 6 and 9).
pub(crate) fn has_active_vertex_property_index(
    graph_id: GraphId,
    label_id: VertexLabelId,
    property_id: PropertyId,
) -> bool {
    let target_label = label_id.raw();
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        map.range((Bound::Included(start), named_index_graph_upper(graph_id)))
            .any(|entry| {
                let def = entry.value();
                def.kind == IndexedPropertyKind::Vertex
                    && def.label_id == target_label
                    && def.property_id == property_id
            })
    })
}

pub(crate) fn load_graph_stats(graph_id: GraphId) -> RouterGraphStats {
    let mut vertex = std::collections::BTreeSet::new();
    let mut edge = std::collections::BTreeSet::new();
    let mut edge_indexes = std::collections::BTreeSet::new();

    ROUTER_INDEXED_PROPERTY_SET.with_borrow(|set| {
        for key in membership_range(graph_id, set) {
            match key.kind() {
                IndexedPropertyKind::Vertex => {
                    vertex.insert(key.property_id);
                }
                IndexedPropertyKind::Edge => {
                    edge.insert(key.property_id);
                }
            }
        }
    });

    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        for entry in map.range((Bound::Included(start), named_index_graph_upper(graph_id))) {
            let def = entry.value();
            if def.kind != IndexedPropertyKind::Edge {
                continue;
            }
            let Some(tag) = tag_from_byte(def.edge_direction_tag) else {
                continue;
            };
            edge_indexes.insert(EdgeIndexMembership {
                property_id: def.property_id,
                label_id: def.label_id,
                direction: tag,
            });
        }
    });

    RouterGraphStats::from_catalog(graph_id, vertex, edge, edge_indexes)
}

pub(crate) fn create_named_index(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    entry: crate::planner_stats::IndexCatalogEntry,
    property_id: PropertyId,
    label_id: u16,
    edge_direction_tag: u8,
    if_not_exists: bool,
) -> Result<(bool, bool), RouterError> {
    let named_key = NamedIndexKey::new(graph_id, index_name_id);
    let exists = ROUTER_NAMED_INDEXES.with_borrow(|map| map.contains_key(&named_key));
    if exists {
        if if_not_exists {
            return Ok((false, false));
        }
        return Err(RouterError::Conflict(format!(
            "index already exists: {index_name_id}"
        )));
    }

    if entry.kind == IndexedPropertyKind::Edge
        && edge_index_identity_exists(graph_id, label_id, property_id, edge_direction_tag)
    {
        return Err(RouterError::Conflict(format!(
            "edge index already exists for label {label_id}, property {property_id}, direction {edge_direction_tag}"
        )));
    }

    let def = IndexDefRecord {
        kind: entry.kind,
        property_id,
        label_id,
        edge_direction_tag,
    };
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        map.insert(named_key, def);
    });

    let membership = IndexedPropertyKey::new(graph_id, entry.kind, property_id);
    let newly_registered = ROUTER_INDEXED_PROPERTY_SET.with_borrow_mut(|set| {
        if set.contains(&membership) {
            false
        } else {
            set.insert(membership);
            true
        }
    });

    Ok((true, newly_registered))
}

pub(crate) fn drop_named_index(
    graph_id: GraphId,
    index_name_id: IndexNameId,
    if_exists: bool,
) -> Result<Option<IndexDefRecord>, RouterError> {
    let named_key = NamedIndexKey::new(graph_id, index_name_id);
    let removed = ROUTER_NAMED_INDEXES.with_borrow_mut(|map| map.remove(&named_key));
    let Some(def) = removed else {
        if if_exists {
            return Ok(None);
        }
        return Err(RouterError::NotFound(index_name_id.to_string()));
    };

    let still_named = ROUTER_NAMED_INDEXES
        .with_borrow(|map| named_index_uses_property(map, graph_id, def.kind, def.property_id));

    if !still_named {
        let membership = IndexedPropertyKey::new(graph_id, def.kind, def.property_id);
        ROUTER_INDEXED_PROPERTY_SET.with_borrow_mut(|set| {
            set.remove(&membership);
        });
    }

    Ok(Some(def))
}

pub(crate) fn is_property_registered(
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: PropertyId,
) -> bool {
    let key = IndexedPropertyKey::new(graph_id, kind, property_id);
    ROUTER_INDEXED_PROPERTY_SET.with_borrow(|set| set.contains(&key))
}

/// Whether any remaining edge index references `(property_id, label_id)` for the
/// graph (any direction). Edge postings carry the catalog `label_id`, so this is
/// the granularity at which a `DROP INDEX` posting purge is safe (ADR 0023 D6).
pub(crate) fn edge_index_uses_property_label(
    graph_id: GraphId,
    property_id: PropertyId,
    label_id: u16,
) -> bool {
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        map.range((Bound::Included(start), named_index_graph_upper(graph_id)))
            .any(|entry| {
                let def = entry.value();
                def.kind == IndexedPropertyKind::Edge
                    && def.property_id == property_id
                    && def.label_id == label_id
            })
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
    ROUTER_INDEXED_PROPERTY_SET.with_borrow_mut(|set| {
        let start = IndexedPropertyKey::new(
            graph_id,
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(0),
        );
        let keys: Vec<_> = set
            .range((Bound::Included(start), membership_graph_upper(graph_id)))
            .collect();
        for key in keys {
            set.remove(&key);
        }
    });
}

/// Exclusive upper bound of one graph's `NamedIndexKey` range. `graph_id` is the most-significant
/// key component, so `[(graph_id, 0), (graph_id + 1, 0))` covers exactly that graph. At
/// `GraphId::MAX` there is no `graph_id + 1`; the bound must be `Unbounded` (every remaining key
/// belongs to the max graph) — a saturating `+1` would collapse to `(MAX, 0)` and yield an empty
/// range, silently dropping the max graph's indexes.
fn named_index_graph_upper(graph_id: GraphId) -> Bound<NamedIndexKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(NamedIndexKey::new(
            GraphId::from_raw(next),
            IndexNameId::from_raw(0),
        )),
        None => Bound::Unbounded,
    }
}

/// Exclusive upper bound of one graph's `IndexedPropertyKey` range. See
/// [`named_index_graph_upper`]; the same `GraphId::MAX` reasoning applies. `Vertex` is the minimum
/// `kind` tag, so the start/end at `kind = Vertex, property = 0` spans both vertex and edge
/// membership keys for the graph.
fn membership_graph_upper(graph_id: GraphId) -> Bound<IndexedPropertyKey> {
    match graph_id.raw().checked_add(1) {
        Some(next) => Bound::Excluded(IndexedPropertyKey::new(
            GraphId::from_raw(next),
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(0),
        )),
        None => Bound::Unbounded,
    }
}

fn edge_index_identity_exists(
    graph_id: GraphId,
    label_id: u16,
    property_id: PropertyId,
    edge_direction_tag: u8,
) -> bool {
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        map.range((Bound::Included(start), named_index_graph_upper(graph_id)))
            .any(|entry| {
                let def = entry.value();
                def.kind == IndexedPropertyKind::Edge
                    && def.label_id == label_id
                    && def.property_id == property_id
                    && def.edge_direction_tag == edge_direction_tag
            })
    })
}

fn named_index_uses_property(
    map: &super::memory::StableNamedIndexMap,
    graph_id: GraphId,
    kind: IndexedPropertyKind,
    property_id: PropertyId,
) -> bool {
    let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
    map.range((Bound::Included(start), named_index_graph_upper(graph_id)))
        .any(|entry| {
            let def = entry.value();
            def.kind == kind && def.property_id == property_id
        })
}

fn membership_range<'a>(
    graph_id: GraphId,
    set: &'a super::memory::StableIndexedPropertySet,
) -> impl Iterator<Item = IndexedPropertyKey> + 'a {
    let start = IndexedPropertyKey::new(
        graph_id,
        IndexedPropertyKind::Vertex,
        PropertyId::from_raw(0),
    );
    set.range((Bound::Included(start), membership_graph_upper(graph_id)))
}

const fn kind_to_byte(kind: IndexedPropertyKind) -> u8 {
    match kind {
        IndexedPropertyKind::Vertex => KIND_VERTEX,
        IndexedPropertyKind::Edge => KIND_EDGE,
    }
}

fn kind_from_byte(byte: u8) -> IndexedPropertyKind {
    match byte {
        KIND_VERTEX => IndexedPropertyKind::Vertex,
        _ => IndexedPropertyKind::Edge,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge_index_direction::EdgeIndexDirectionTag;

    #[test]
    fn named_index_key_storable_roundtrip() {
        let key = NamedIndexKey::new(GraphId::from_raw(1), IndexNameId::from_raw(2));
        let decoded = NamedIndexKey::from_bytes(Cow::Owned(key.into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn membership_key_storable_roundtrip() {
        let key = IndexedPropertyKey::new(
            GraphId::from_raw(1),
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7),
        );
        let decoded = IndexedPropertyKey::from_bytes(Cow::Owned(key.into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn index_def_record_storable_roundtrip() {
        let record = IndexDefRecord {
            kind: IndexedPropertyKind::Edge,
            property_id: PropertyId::from_raw(42),
            label_id: 3,
            edge_direction_tag: EdgeIndexDirectionTag::AnyDirection as u8,
        };
        let decoded = IndexDefRecord::from_bytes(Cow::Owned(record.into_bytes()));
        assert_eq!(decoded, record);
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
    fn vertex_drop_unregisters_property_only_when_last_index_gone() {
        let graph = GraphId::from_raw(700_001);
        let property = PropertyId::from_raw(11);
        // Two vertex indexes on the same property (different labels) share postings.
        create_named_index(
            graph,
            IndexNameId::from_raw(1),
            vertex_entry(),
            property,
            1,
            0,
            false,
        )
        .expect("create idx1");
        create_named_index(
            graph,
            IndexNameId::from_raw(2),
            vertex_entry(),
            property,
            2,
            0,
            false,
        )
        .expect("create idx2");
        assert!(is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            property
        ));

        drop_named_index(graph, IndexNameId::from_raw(1), false).expect("drop idx1");
        // A vertex index still references the property → no purge yet.
        assert!(is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            property
        ));

        drop_named_index(graph, IndexNameId::from_raw(2), false).expect("drop idx2");
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            property
        ));
    }

    #[test]
    fn edge_drop_scopes_purge_to_property_label() {
        let graph = GraphId::from_raw(700_002);
        let property = PropertyId::from_raw(21);
        let knows = 3u16;
        let likes = 4u16;
        create_named_index(
            graph,
            IndexNameId::from_raw(1),
            edge_entry(),
            property,
            knows,
            1,
            false,
        )
        .expect("create KNOWS idx");
        create_named_index(
            graph,
            IndexNameId::from_raw(2),
            edge_entry(),
            property,
            likes,
            1,
            false,
        )
        .expect("create LIKES idx");

        let def = drop_named_index(graph, IndexNameId::from_raw(1), false)
            .expect("drop KNOWS idx")
            .expect("removed def");
        assert_eq!(def.label_id, knows);
        // (property, KNOWS) has no remaining index → its postings can be purged.
        assert!(!edge_index_uses_property_label(graph, property, knows));
        // (property, LIKES) is still indexed → must not be purged.
        assert!(edge_index_uses_property_label(graph, property, likes));
    }

    #[test]
    fn range_scans_cover_the_max_graph_id() {
        // Regression: a saturating `graph_id + 1` upper bound collapses to an empty range at
        // GraphId::MAX, so every range scan (stats load, edge-label lookup, drop's
        // last-reference check, purge) would silently skip the max graph. Exercise search,
        // delete, and purge on GraphId::MAX with the `Unbounded` upper bound in place.
        let graph = GraphId::from_raw(u32::MAX);
        let vprop = PropertyId::from_raw(11);
        let eprop = PropertyId::from_raw(21);
        let knows = 3u16;

        create_named_index(
            graph,
            IndexNameId::from_raw(1),
            vertex_entry(),
            vprop,
            1,
            0,
            false,
        )
        .expect("create vertex idx");
        create_named_index(
            graph,
            IndexNameId::from_raw(2),
            edge_entry(),
            eprop,
            knows,
            1,
            false,
        )
        .expect("create edge idx");

        // Search: range-backed stats load must see both indexed properties.
        let stats = load_graph_stats(graph);
        assert!(
            stats.is_vertex_property_id_indexed(vprop),
            "vertex index on max graph must load, not be skipped"
        );
        assert!(
            stats.is_edge_property_id_indexed(eprop),
            "edge index on max graph must load, not be skipped"
        );
        assert!(edge_index_uses_property_label(graph, eprop, knows));

        // Delete: drop's range-backed last-reference check must unregister membership.
        drop_named_index(graph, IndexNameId::from_raw(1), false).expect("drop vertex idx");
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Vertex,
            vprop
        ));

        // Purge: both range scans (named indexes + membership set) must clear the max graph.
        purge_graph_indexes(graph);
        let after = load_graph_stats(graph);
        assert!(!after.is_edge_property_id_indexed(eprop));
        assert!(!is_property_registered(
            graph,
            IndexedPropertyKind::Edge,
            eprop
        ));
        assert!(!edge_index_uses_property_label(graph, eprop, knows));
    }
}
