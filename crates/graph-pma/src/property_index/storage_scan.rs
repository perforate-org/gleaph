use std::ops::Bound;

use crate::low_level::RegionManager;
use crate::stable::Memory;

use super::super::property_equality::decode_pidx_v3_region;
use super::super::{
    PIDX_V3_MAGIC, PropertyEqualityStableMap, PropertyIndexEntityKind, PropertyIndexEntry,
    PropertyIndexError, PropertyIndexKey, empty_property_equality_map,
};
use super::region_io::{read_property_index_region_bytes, read_property_index_region_magic};

fn load_map_from_region(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyEqualityStableMap, PropertyIndexError> {
    match read_property_index_region_magic(manager, memory)? {
        Some(m) if m == PIDX_V3_MAGIC => {
            let bytes = read_property_index_region_bytes(manager, memory)?;
            decode_pidx_v3_region(&bytes)
        }
        _ => Ok(empty_property_equality_map()),
    }
}

pub fn scan_node_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_value_prefix_from_stable_memory(
        manager,
        memory,
        PropertyIndexEntityKind::VertexNode,
        property,
        encoded_value,
    )
}

pub fn scan_edge_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_value_prefix_from_stable_memory(
        manager,
        memory,
        PropertyIndexEntityKind::VertexEdge,
        property,
        encoded_value,
    )
}

pub fn scan_node_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_property_prefix_from_stable_memory(
        manager,
        memory,
        PropertyIndexEntityKind::VertexNode,
        property,
    )
}

pub fn scan_edge_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_property_prefix_from_stable_memory(
        manager,
        memory,
        PropertyIndexEntityKind::VertexEdge,
        property,
    )
}

fn scan_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    entity_kind: PropertyIndexEntityKind,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    let map = load_map_from_region(manager, memory)?;
    let start = PropertyIndexKey::lower_bound(entity_kind, property, encoded_value.to_vec());
    let end = PropertyIndexKey::btree_property_range_end_exclusive(entity_kind, property);
    let mut out = Vec::new();
    match end {
        Some(ex) => {
            for e in map.range((Bound::Included(start), Bound::Excluded(ex))) {
                let k = e.key();
                if k.matches_value_prefix(entity_kind, property, encoded_value) {
                    out.push((k.clone(), e.value()));
                }
            }
        }
        None => {
            for e in map.range((Bound::Included(start.clone()), Bound::Unbounded)) {
                let k = e.key();
                if k.matches_value_prefix(entity_kind, property, encoded_value) {
                    out.push((k.clone(), e.value()));
                } else if *k > start && !k.matches_property_prefix(entity_kind, property) {
                    break;
                }
            }
        }
    }
    Ok(out)
}

fn scan_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    entity_kind: PropertyIndexEntityKind,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    let map = load_map_from_region(manager, memory)?;
    let start = PropertyIndexKey::btree_property_range_start(entity_kind, property);
    let end = PropertyIndexKey::btree_property_range_end_exclusive(entity_kind, property);
    let mut out = Vec::new();
    match end {
        Some(ex) => {
            for e in map.range((Bound::Included(start), Bound::Excluded(ex))) {
                out.push((e.key().clone(), e.value()));
            }
        }
        None => {
            for e in map.range((Bound::Included(start), Bound::Unbounded)) {
                let k = e.key();
                if k.matches_property_prefix(entity_kind, property) {
                    out.push((k.clone(), e.value()));
                } else {
                    break;
                }
            }
        }
    }
    Ok(out)
}
