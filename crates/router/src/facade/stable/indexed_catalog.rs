//! Row-oriented index catalog in stable memory (ADR 0009 §4, ADR 0011 id keys).
//!
//! - `ROUTER_NAMED_INDEXES`: `(graph_id, index_name_id) → IndexDefRecord`
//! - `ROUTER_INDEXED_PROPERTY_SET`: `(graph_id, kind, property_id)` membership for planner + fan-out

use std::borrow::Cow;
use std::ops::Bound;

use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
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
        let end = NamedIndexKey::new(graph_id_upper_bound(graph_id), IndexNameId::from_raw(0));
        for entry in map.range((Bound::Included(start), Bound::Excluded(end))) {
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

    RouterGraphStats::from_catalog(vertex, edge, edge_indexes)
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

fn graph_id_upper_bound(graph_id: GraphId) -> GraphId {
    GraphId::from_raw(graph_id.raw().saturating_add(1))
}

fn edge_index_identity_exists(
    graph_id: GraphId,
    label_id: u16,
    property_id: PropertyId,
    edge_direction_tag: u8,
) -> bool {
    ROUTER_NAMED_INDEXES.with_borrow(|map| {
        let start = NamedIndexKey::new(graph_id, IndexNameId::from_raw(0));
        let end = NamedIndexKey::new(graph_id_upper_bound(graph_id), IndexNameId::from_raw(0));
        map.range((Bound::Included(start), Bound::Excluded(end)))
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
    let end = NamedIndexKey::new(graph_id_upper_bound(graph_id), IndexNameId::from_raw(0));
    map.range((Bound::Included(start), Bound::Excluded(end)))
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
    let end = IndexedPropertyKey::new(
        graph_id_upper_bound(graph_id),
        IndexedPropertyKind::Vertex,
        PropertyId::from_raw(0),
    );
    set.range((Bound::Included(start), Bound::Excluded(end)))
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
}
