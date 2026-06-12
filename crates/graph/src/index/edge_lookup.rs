//! Edge property equality lookups via graph-index or canonical `EDGE_PROPERTIES` scan.

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;

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
                label_id: hit.label_id,
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
