//! Edge index direction tags and storage-class lattice (ADR 0012).

use gleaph_gql::types::EdgeDirection;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};

/// Stable tag stored in router `IndexDefRecord` and graph shard registrations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum EdgeIndexDirectionTag {
    PointingRight = 1,
    PointingLeft = 2,
    LeftOrRight = 3,
    Undirected = 4,
    UndirectedOrRight = 5,
    LeftOrUndirected = 6,
    AnyDirection = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageClass {
    Directed,
    Undirected,
}

const DIRECTED_ONLY: &[StorageClass] = &[StorageClass::Directed];
const UNDIRECTED_ONLY: &[StorageClass] = &[StorageClass::Undirected];
const BOTH: &[StorageClass] = &[StorageClass::Directed, StorageClass::Undirected];

pub fn direction_tag(direction: EdgeDirection) -> EdgeIndexDirectionTag {
    match direction {
        EdgeDirection::PointingRight => EdgeIndexDirectionTag::PointingRight,
        EdgeDirection::PointingLeft => EdgeIndexDirectionTag::PointingLeft,
        EdgeDirection::LeftOrRight => EdgeIndexDirectionTag::LeftOrRight,
        EdgeDirection::Undirected => EdgeIndexDirectionTag::Undirected,
        EdgeDirection::UndirectedOrRight => EdgeIndexDirectionTag::UndirectedOrRight,
        EdgeDirection::LeftOrUndirected => EdgeIndexDirectionTag::LeftOrUndirected,
        EdgeDirection::AnyDirection => EdgeIndexDirectionTag::AnyDirection,
    }
}

pub fn tag_to_direction(tag: EdgeIndexDirectionTag) -> EdgeDirection {
    match tag {
        EdgeIndexDirectionTag::PointingRight => EdgeDirection::PointingRight,
        EdgeIndexDirectionTag::PointingLeft => EdgeDirection::PointingLeft,
        EdgeIndexDirectionTag::LeftOrRight => EdgeDirection::LeftOrRight,
        EdgeIndexDirectionTag::Undirected => EdgeDirection::Undirected,
        EdgeIndexDirectionTag::UndirectedOrRight => EdgeDirection::UndirectedOrRight,
        EdgeIndexDirectionTag::LeftOrUndirected => EdgeDirection::LeftOrUndirected,
        EdgeIndexDirectionTag::AnyDirection => EdgeDirection::AnyDirection,
    }
}

pub fn tag_from_byte(byte: u8) -> Option<EdgeIndexDirectionTag> {
    match byte {
        1 => Some(EdgeIndexDirectionTag::PointingRight),
        2 => Some(EdgeIndexDirectionTag::PointingLeft),
        3 => Some(EdgeIndexDirectionTag::LeftOrRight),
        4 => Some(EdgeIndexDirectionTag::Undirected),
        5 => Some(EdgeIndexDirectionTag::UndirectedOrRight),
        6 => Some(EdgeIndexDirectionTag::LeftOrUndirected),
        7 => Some(EdgeIndexDirectionTag::AnyDirection),
        _ => None,
    }
}

fn storage_classes(direction: EdgeDirection) -> &'static [StorageClass] {
    match direction {
        EdgeDirection::PointingRight | EdgeDirection::PointingLeft | EdgeDirection::LeftOrRight => {
            DIRECTED_ONLY
        }
        EdgeDirection::Undirected => UNDIRECTED_ONLY,
        EdgeDirection::AnyDirection
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight => BOTH,
    }
}

fn storage_class_is_subset(query: &[StorageClass], index: &[StorageClass]) -> bool {
    query.iter().all(|q| index.contains(q))
}

/// Query direction `Q` may use an index registered with direction `I`.
pub fn index_applies_to_query(
    index_direction: EdgeDirection,
    query_direction: EdgeDirection,
) -> bool {
    storage_class_is_subset(
        storage_classes(query_direction),
        storage_classes(index_direction),
    )
}

pub fn wire_label_for_storage(catalog: EdgeLabelId, class: StorageClass) -> u16 {
    let directedness = match class {
        StorageClass::Directed => EdgeDirectedness::Directed,
        StorageClass::Undirected => EdgeDirectedness::Undirected,
    };
    catalog.pack(directedness).raw()
}

pub fn storage_class_from_wire(wire_label_id: u16) -> Option<StorageClass> {
    const BUCKET_LABEL_DIRECTED_BIT: u16 = 0x8000;
    if wire_label_id & BUCKET_LABEL_DIRECTED_BIT != 0 {
        Some(StorageClass::Directed)
    } else if wire_label_id == 0 {
        None
    } else {
        Some(StorageClass::Undirected)
    }
}

pub fn wire_labels_for_query(catalog: EdgeLabelId, query_direction: EdgeDirection) -> Vec<u16> {
    storage_classes(query_direction)
        .iter()
        .map(|class| wire_label_for_storage(catalog, *class))
        .collect()
}

pub fn edge_posting_matches_registration(
    catalog: EdgeLabelId,
    wire_label_id: u16,
    index_label_id: u16,
    index_direction: EdgeDirection,
) -> bool {
    if catalog.raw() != index_label_id {
        return false;
    }
    let Some(edge_class) = storage_class_from_wire(wire_label_id) else {
        return false;
    };
    storage_classes(index_direction).contains(&edge_class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_index_covers_pointing_right_query() {
        assert!(index_applies_to_query(
            EdgeDirection::AnyDirection,
            EdgeDirection::PointingRight,
        ));
    }

    #[test]
    fn pointing_right_index_does_not_cover_any_query() {
        assert!(!index_applies_to_query(
            EdgeDirection::PointingRight,
            EdgeDirection::AnyDirection,
        ));
    }

    #[test]
    fn wire_labels_for_pointing_right_use_directed_bucket() {
        let catalog = EdgeLabelId::from_raw(1);
        let wires = wire_labels_for_query(catalog, EdgeDirection::PointingRight);
        assert_eq!(wires, vec![0x8001]);
    }

    #[test]
    fn wire_labels_for_any_use_both_buckets() {
        let catalog = EdgeLabelId::from_raw(1);
        let mut wires = wire_labels_for_query(catalog, EdgeDirection::AnyDirection);
        wires.sort_unstable();
        assert_eq!(wires, vec![0x0001, 0x8001]);
    }
}
