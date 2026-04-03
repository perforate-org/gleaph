use std::collections::BTreeSet;

use crate::low_level::RegionManager;
use crate::stable::Memory;

use crate::property_index::{
    PropertyIndexEntityKind, PropertyIndexEntry, PropertyIndexError, PropertyIndexKey,
    PropertyIndexNodeId, PropertyIndexNodeRecord, PropertyIndexNodeStore,
};

use super::region_io::{
    PropertyIndexPagedAreaMetadata, read_edge_property_index_node_record_from_stable_memory,
    read_node_property_index_node_record_from_stable_memory,
    read_property_index_paged_area_metadata_from_stable_memory,
};

pub fn scan_node_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_value_prefix_from_stable_memory(
        manager,
        memory,
        true,
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
        false,
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
        true,
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
        false,
        PropertyIndexEntityKind::VertexEdge,
        property,
    )
}

fn scan_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    entity_kind: PropertyIndexEntityKind,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    let target = PropertyIndexKey::lower_bound(entity_kind, property, encoded_value.to_vec());
    let metadata =
        read_property_index_paged_area_metadata_from_stable_memory(manager, memory, node_side)?;
    let Some(mut leaf_id) = find_property_index_leaf_for_key_from_stable_memory(
        manager, memory, node_side, &target, metadata,
    )?
    else {
        return Ok(Vec::new());
    };

    let mut visited = BTreeSet::new();
    let mut out = Vec::new();
    loop {
        if !visited.insert(leaf_id) {
            break;
        }
        let record = read_node_record(manager, memory, node_side, leaf_id)?;
        let PropertyIndexNodeRecord::Leaf { header, entries } = record else {
            break;
        };
        let mut saw_matching_prefix = false;
        let mut should_stop = false;
        for (key, entry) in entries {
            if key.matches_value_prefix(entity_kind, property, encoded_value) {
                saw_matching_prefix = true;
                out.push((key, entry));
            } else if saw_matching_prefix || key > target {
                should_stop = true;
                if saw_matching_prefix {
                    break;
                }
            }
        }
        if should_stop || header.next_leaf.is_null() {
            break;
        }
        leaf_id = header.next_leaf;
    }
    Ok(out)
}

fn scan_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    entity_kind: PropertyIndexEntityKind,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    let target = PropertyIndexKey::property_lower_bound(entity_kind, property);
    let metadata =
        read_property_index_paged_area_metadata_from_stable_memory(manager, memory, node_side)?;
    let Some(mut leaf_id) = find_property_index_leaf_for_key_from_stable_memory(
        manager, memory, node_side, &target, metadata,
    )?
    else {
        return Ok(Vec::new());
    };

    let mut visited = BTreeSet::new();
    let mut out = Vec::new();
    loop {
        if !visited.insert(leaf_id) {
            break;
        }
        let record = read_node_record(manager, memory, node_side, leaf_id)?;
        let PropertyIndexNodeRecord::Leaf { header, entries } = record else {
            break;
        };
        let mut saw_matching_prefix = false;
        let mut should_stop = false;
        for (key, entry) in entries {
            if key.matches_property_prefix(entity_kind, property) {
                saw_matching_prefix = true;
                out.push((key, entry));
            } else if saw_matching_prefix || key > target {
                should_stop = true;
                if saw_matching_prefix {
                    break;
                }
            }
        }
        if should_stop || header.next_leaf.is_null() {
            break;
        }
        leaf_id = header.next_leaf;
    }
    Ok(out)
}

fn find_property_index_leaf_for_key_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    target: &PropertyIndexKey,
    metadata: PropertyIndexPagedAreaMetadata,
) -> Result<Option<PropertyIndexNodeId>, PropertyIndexError> {
    let Some(mut current) =
        infer_property_index_root_id_from_stable_memory(manager, memory, node_side, metadata)?
    else {
        return Ok(None);
    };
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current) {
            return Ok(None);
        }
        let record = read_node_record(manager, memory, node_side, current)?;
        match record {
            PropertyIndexNodeRecord::Leaf { .. } => return Ok(Some(current)),
            PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                let child_index =
                    PropertyIndexNodeStore::select_child_for_key(&keys, children.len(), target);
                let Some(next) = children.get(child_index).copied() else {
                    return Ok(None);
                };
                current = next;
            }
        }
    }
}

fn infer_property_index_root_id_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    metadata: PropertyIndexPagedAreaMetadata,
) -> Result<Option<PropertyIndexNodeId>, PropertyIndexError> {
    let mut internal_ids = BTreeSet::new();
    let mut referenced_internal_ids = BTreeSet::new();
    let mut fallback_first_leaf = None;

    for raw in 1..=metadata.page_count {
        let node_id = PropertyIndexNodeId(raw as u64);
        let record = match read_node_record(manager, memory, node_side, node_id) {
            Ok(record) => record,
            Err(PropertyIndexError::MissingNodeSlot(_)) => continue,
            Err(err) => return Err(err),
        };
        match record {
            PropertyIndexNodeRecord::Internal { children, .. } => {
                internal_ids.insert(node_id);
                for child_id in children {
                    referenced_internal_ids.insert(child_id);
                }
            }
            PropertyIndexNodeRecord::Leaf { header, .. } => {
                if fallback_first_leaf.is_none() && header.prev_leaf.is_null() {
                    fallback_first_leaf = Some(node_id);
                }
            }
        }
    }

    if let Some(root_id) = internal_ids
        .iter()
        .find(|node_id| !referenced_internal_ids.contains(node_id))
        .copied()
        .or_else(|| internal_ids.iter().next().copied())
    {
        return Ok(Some(root_id));
    }

    Ok(fallback_first_leaf)
}

fn read_node_record(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    if node_side {
        read_node_property_index_node_record_from_stable_memory(manager, memory, node_id)
    } else {
        read_edge_property_index_node_record_from_stable_memory(manager, memory, node_id)
    }
}
