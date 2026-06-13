//! Edge property equality lookups via graph-index or canonical `EDGE_PROPERTIES` scan.

use crate::facade::GraphStore;
use crate::facade::catalog_edge_label_from_wire;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::BUCKET_LABEL_DIRECTED_BIT;

/// Catalog edge label id for graph-index postings (router seed lookup uses catalog ids).
pub(crate) fn catalog_label_id_for_index_posting(wire_label_id: u16) -> u16 {
    let wire = LaraLabelId::from_raw(wire_label_id);
    catalog_edge_label_from_wire(wire)
        .map(|id| id.raw())
        .unwrap_or(wire_label_id)
}

/// CSR wire label for local edge handles after reading catalog ids from graph-index.
pub(crate) fn wire_label_id_for_local_edge(stored_label_id: u16) -> u16 {
    if stored_label_id & BUCKET_LABEL_DIRECTED_BIT != 0 || stored_label_id == 0 {
        stored_label_id
    } else {
        LaraLabelId::directed_from_index(stored_label_id).raw()
    }
}

/// Shard-local edge identity for expand / edge-index execution paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalEdgePosting {
    pub owner_vertex_id: VertexId,
    pub label_id: u16,
    pub slot_index: u32,
}

pub(crate) async fn lookup_edge_equal_local(
    index: Option<&dyn PropertyIndexLookup>,
    property_id: PropertyId,
    expected: &[u8],
    label_id: Option<u16>,
) -> Result<Vec<LocalEdgePosting>, PlanQueryError> {
    if let Some(ix) = index {
        let hits = ix
            .lookup_edge_equal(property_id.raw(), expected.to_vec(), label_id)
            .await?;
        let shard_id = ix.local_shard_id();
        return Ok(hits
            .into_iter()
            .filter(|hit| hit.shard_id == shard_id)
            .map(|hit| LocalEdgePosting {
                owner_vertex_id: VertexId::from(hit.owner_vertex_id),
                label_id: wire_label_id_for_local_edge(hit.label_id),
                slot_index: hit.slot_index,
            })
            .collect());
    }
    Ok(scan_store_edge_equal(property_id, expected, label_id))
}

pub(crate) fn lookup_edge_equal_local_sync(
    index: Option<&dyn PropertyIndexLookup>,
    property_id: PropertyId,
    expected: &[u8],
    label_id: Option<u16>,
) -> Result<Vec<LocalEdgePosting>, PlanQueryError> {
    if let Some(ix) = index {
        return pollster::block_on(lookup_edge_equal_local(
            Some(ix),
            property_id,
            expected,
            label_id,
        ));
    }
    Ok(scan_store_edge_equal(property_id, expected, label_id))
}

fn scan_store_edge_equal(
    property_id: PropertyId,
    expected: &[u8],
    label_id: Option<u16>,
) -> Vec<LocalEdgePosting> {
    GraphStore::collect_edges_matching_indexed_property(property_id, expected, label_id)
        .into_iter()
        .map(|(owner_vertex_id, label_id, slot_index)| LocalEdgePosting {
            owner_vertex_id,
            label_id,
            slot_index,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};

    #[test]
    fn catalog_label_id_for_index_posting_strips_directed_wire_bit() {
        let catalog = EdgeLabelId::from_raw(1);
        let wire = catalog.pack(EdgeDirectedness::Directed).raw();
        assert_eq!(catalog_label_id_for_index_posting(wire), catalog.raw());
    }

    #[test]
    fn wire_label_id_for_local_edge_restores_directed_wire_from_catalog() {
        assert_eq!(wire_label_id_for_local_edge(1), 0x8001);
        assert_eq!(wire_label_id_for_local_edge(0x8001), 0x8001);
    }
}
