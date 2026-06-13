//! Row-oriented index catalog in stable memory (ADR 0009 §4).
//!
//! - `ROUTER_NAMED_INDEXES`: `(graph, index_name) → IndexDefRecord`
//! - `ROUTER_INDEXED_PROPERTY_SET`: `(graph, kind, property_id)` membership for planner + fan-out

use std::borrow::Cow;
use std::ops::Bound;

use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::index::IndexedPropertyKind;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};

use crate::facade::stable::{ROUTER_INDEXED_PROPERTY_SET, ROUTER_NAMED_INDEXES};
use crate::facade::store::RouterStore;
use crate::planner_stats::{IndexCatalogEntry, RouterGraphStats};
use crate::state::RouterError;

const KIND_VERTEX: u8 = 0;
const KIND_EDGE: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct NamedIndexKey {
    pub graph: String,
    pub index_name: String,
}

impl NamedIndexKey {
    pub fn new(graph: impl Into<String>, index_name: impl Into<String>) -> Self {
        Self {
            graph: graph.into(),
            index_name: index_name.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IndexedPropertyKey {
    pub graph: String,
    kind_tag: u8,
    pub property_id: PropertyId,
}

impl IndexedPropertyKey {
    pub fn new(
        graph: impl Into<String>,
        kind: IndexedPropertyKind,
        property_id: PropertyId,
    ) -> Self {
        Self {
            graph: graph.into(),
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
}

impl Storable for NamedIndexKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(encode_two_strings(&self.graph, &self.index_name))
    }

    fn into_bytes(self) -> Vec<u8> {
        encode_two_strings(&self.graph, &self.index_name)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let (graph, index_name) = decode_two_strings(bytes.as_ref());
        Self { graph, index_name }
    }
}

impl Storable for IndexedPropertyKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(encode_membership_key_tagged(
            &self.graph,
            self.kind_tag,
            self.property_id,
        ))
    }

    fn into_bytes(self) -> Vec<u8> {
        encode_membership_key_tagged(&self.graph, self.kind_tag, self.property_id)
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let (graph, kind_tag, property_id) = decode_membership_key_tagged(bytes.as_ref());
        Self {
            graph,
            kind_tag,
            property_id,
        }
    }
}

impl Storable for IndexDefRecord {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 7,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(7);
        out.push(kind_to_byte(self.kind));
        out.extend_from_slice(&self.property_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        Self {
            kind: kind_from_byte(bytes[0]),
            property_id: PropertyId::from_le_bytes(bytes[1..5].try_into().expect("property_id")),
            label_id: u16::from_le_bytes(bytes[5..7].try_into().expect("label_id")),
        }
    }
}

pub(crate) fn build_graph_stats(logical_graph_name: &str, store: &RouterStore) -> RouterGraphStats {
    let mut vertex = std::collections::BTreeSet::new();
    let mut edge = std::collections::BTreeSet::new();

    ROUTER_INDEXED_PROPERTY_SET.with_borrow(|set| {
        for key in membership_range(logical_graph_name, set) {
            let Ok(name) = store.reverse_property_name(key.property_id) else {
                continue;
            };
            match key.kind() {
                IndexedPropertyKind::Vertex => {
                    vertex.insert(name);
                }
                IndexedPropertyKind::Edge => {
                    edge.insert(name);
                }
            }
        }
    });

    RouterGraphStats::from_indexed_properties(vertex, edge)
}

pub(crate) fn create_named_index(
    logical_graph_name: &str,
    index_name: &str,
    entry: IndexCatalogEntry,
    property_id: PropertyId,
    label_id: u16,
    if_not_exists: bool,
) -> Result<bool, RouterError> {
    let named_key = NamedIndexKey::new(logical_graph_name, index_name);
    let exists = ROUTER_NAMED_INDEXES.with_borrow(|map| map.contains_key(&named_key));
    if exists {
        if if_not_exists {
            return Ok(false);
        }
        return Err(RouterError::Conflict(format!(
            "index already exists: {index_name}"
        )));
    }

    let def = IndexDefRecord {
        kind: entry.kind,
        property_id,
        label_id,
    };
    ROUTER_NAMED_INDEXES.with_borrow_mut(|map| {
        map.insert(named_key, def);
    });

    let membership = IndexedPropertyKey::new(logical_graph_name, entry.kind, property_id);
    let newly_registered = ROUTER_INDEXED_PROPERTY_SET.with_borrow_mut(|set| {
        if set.contains(&membership) {
            false
        } else {
            set.insert(membership);
            true
        }
    });

    Ok(newly_registered)
}

pub(crate) fn drop_named_index(
    logical_graph_name: &str,
    index_name: &str,
    if_exists: bool,
) -> Result<Option<(IndexedPropertyKind, PropertyId)>, RouterError> {
    let named_key = NamedIndexKey::new(logical_graph_name, index_name);
    let removed = ROUTER_NAMED_INDEXES.with_borrow_mut(|map| map.remove(&named_key));
    let Some(def) = removed else {
        if if_exists {
            return Ok(None);
        }
        return Err(RouterError::NotFound(index_name.to_string()));
    };

    let still_named = ROUTER_NAMED_INDEXES.with_borrow(|map| {
        named_index_uses_property(map, logical_graph_name, def.kind, def.property_id)
    });

    if !still_named {
        let membership = IndexedPropertyKey::new(logical_graph_name, def.kind, def.property_id);
        ROUTER_INDEXED_PROPERTY_SET.with_borrow_mut(|set| {
            set.remove(&membership);
        });
    }

    Ok(Some((def.kind, def.property_id)))
}

pub(crate) fn is_property_registered(
    logical_graph_name: &str,
    kind: IndexedPropertyKind,
    property_id: PropertyId,
) -> bool {
    let key = IndexedPropertyKey::new(logical_graph_name, kind, property_id);
    ROUTER_INDEXED_PROPERTY_SET.with_borrow(|set| set.contains(&key))
}

pub(crate) fn register_property_membership(
    logical_graph_name: &str,
    kind: IndexedPropertyKind,
    property_id: PropertyId,
) -> bool {
    let key = IndexedPropertyKey::new(logical_graph_name, kind, property_id);
    ROUTER_INDEXED_PROPERTY_SET.with_borrow_mut(|set| {
        if set.contains(&key) {
            false
        } else {
            set.insert(key);
            true
        }
    })
}

fn graph_upper_bound(graph: &str) -> String {
    format!("{graph}\0")
}

fn named_index_uses_property(
    map: &super::memory::StableNamedIndexMap,
    graph: &str,
    kind: IndexedPropertyKind,
    property_id: PropertyId,
) -> bool {
    let start = NamedIndexKey::new(graph, "");
    let end = NamedIndexKey::new(graph_upper_bound(graph), "");
    map.range((Bound::Included(start), Bound::Excluded(end)))
        .any(|entry| {
            let def = entry.value();
            def.kind == kind && def.property_id == property_id
        })
}

fn membership_range<'a>(
    graph: &str,
    set: &'a super::memory::StableIndexedPropertySet,
) -> impl Iterator<Item = IndexedPropertyKey> + 'a {
    let start =
        IndexedPropertyKey::new(graph, IndexedPropertyKind::Vertex, PropertyId::from_raw(0));
    let end = IndexedPropertyKey::new(
        graph_upper_bound(graph),
        IndexedPropertyKind::Vertex,
        PropertyId::from_raw(0),
    );
    set.range((Bound::Included(start), Bound::Excluded(end)))
}

fn encode_two_strings(first: &str, second: &str) -> Vec<u8> {
    let first_bytes = first.as_bytes();
    let second_bytes = second.as_bytes();
    debug_assert!(first_bytes.len() <= u16::MAX as usize);
    debug_assert!(second_bytes.len() <= u16::MAX as usize);
    let mut out = Vec::with_capacity(4 + first_bytes.len() + second_bytes.len());
    out.extend_from_slice(&(first_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(first_bytes);
    out.extend_from_slice(&(second_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(second_bytes);
    out
}

fn decode_two_strings(bytes: &[u8]) -> (String, String) {
    let first_len = u16::from_le_bytes(bytes[0..2].try_into().expect("first_len")) as usize;
    let first_end = 2 + first_len;
    let second_len = u16::from_le_bytes(
        bytes[first_end..first_end + 2]
            .try_into()
            .expect("second_len"),
    ) as usize;
    let second_start = first_end + 2;
    let second_end = second_start + second_len;
    let first = String::from_utf8(bytes[2..first_end].to_vec()).expect("graph utf8");
    let second = String::from_utf8(bytes[second_start..second_end].to_vec()).expect("name utf8");
    (first, second)
}

fn encode_membership_key_tagged(graph: &str, kind_tag: u8, property_id: PropertyId) -> Vec<u8> {
    let graph_bytes = graph.as_bytes();
    debug_assert!(graph_bytes.len() <= u16::MAX as usize);
    let mut out = Vec::with_capacity(2 + graph_bytes.len() + 1 + 4);
    out.extend_from_slice(&(graph_bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(graph_bytes);
    out.push(kind_tag);
    out.extend_from_slice(&property_id.to_le_bytes());
    out
}

fn decode_membership_key_tagged(bytes: &[u8]) -> (String, u8, PropertyId) {
    let graph_len = u16::from_le_bytes(bytes[0..2].try_into().expect("graph_len")) as usize;
    let graph_end = 2 + graph_len;
    let kind_tag = bytes[graph_end];
    let property_id = PropertyId::from_le_bytes(
        bytes[graph_end + 1..graph_end + 5]
            .try_into()
            .expect("property_id"),
    );
    let graph = String::from_utf8(bytes[2..graph_end].to_vec()).expect("graph utf8");
    (graph, kind_tag, property_id)
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
    use gleaph_graph_kernel::entry::PropertyId;
    use std::cmp::Ordering;

    #[test]
    fn named_index_key_storable_roundtrip() {
        let key = NamedIndexKey::new("tenant.main", "person_age");
        let decoded = NamedIndexKey::from_bytes(Cow::Owned(key.clone().into_bytes()));
        assert_eq!(decoded, key);
    }

    #[test]
    fn membership_key_storable_roundtrip() {
        let key = IndexedPropertyKey::new(
            "tenant.main",
            IndexedPropertyKind::Vertex,
            PropertyId::from_raw(7),
        );
        let decoded = IndexedPropertyKey::from_bytes(Cow::Owned(key.into_bytes()));
        assert_eq!(
            decoded,
            IndexedPropertyKey::new(
                "tenant.main",
                IndexedPropertyKind::Vertex,
                PropertyId::from_raw(7),
            )
        );
    }

    #[test]
    fn index_def_record_storable_roundtrip() {
        let record = IndexDefRecord {
            kind: IndexedPropertyKind::Edge,
            property_id: PropertyId::from_raw(42),
            label_id: 3,
        };
        let decoded = IndexDefRecord::from_bytes(Cow::Owned(record.into_bytes()));
        assert_eq!(decoded, record);
    }

    #[test]
    fn graph_upper_bound_orders_after_graph_prefix() {
        assert!(graph_upper_bound("tenant.main").as_str().cmp("tenant.main") == Ordering::Greater);
        assert!(
            graph_upper_bound("tenant.main")
                .as_str()
                .cmp("tenant.mainx")
                == Ordering::Less
        );
    }
}
