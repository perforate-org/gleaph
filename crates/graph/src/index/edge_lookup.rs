//! Edge property equality lookups via graph-index or canonical `EDGE_PROPERTIES` scan.
//!
//! Label identity (GAP-2026-08-22-001): lookup callers and the index canister's sieve speak
//! catalog ids; stored postings are wire-tagged. Membership resolution on this path matches
//! catalog labels directly; translation between the spaces is owned by
//! `gleaph_graph_kernel::entry`.

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId, PropertyId};
use gleaph_graph_kernel::index::{EdgePostingHit, PostingRangeRequest};
use ic_stable_lara::VertexId;

/// Both LARA bucket packings of one catalog label (kernel-owned identity rule): the
/// undirected packing sorts before the directed one. The canonical-store fallback scans
/// storage keys per packing because `EDGE_PROPERTIES` keys are wire-tagged.
fn wire_packings(catalog_label_id: u16) -> [u16; 2] {
    let label = EdgeLabelId::from_raw(catalog_label_id);
    [
        label.pack(EdgeDirectedness::Undirected).raw(),
        label.pack(EdgeDirectedness::Directed).raw(),
    ]
}

/// Label arguments for one canonical-store scan pass: both packings for a catalog label,
/// or the single unrestricted pass.
fn store_scan_labels(catalog_label_id: Option<u16>) -> Vec<Option<u16>> {
    match catalog_label_id {
        Some(catalog) => wire_packings(catalog).into_iter().map(Some).collect(),
        None => vec![None],
    }
}

/// Shard-local edge identity for expand / edge-index execution paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalEdgePosting {
    pub owner_vertex_id: VertexId,
    /// LARA wire tag of the stored posting; doubles as the CSR bucket key for handle binding.
    pub label_id: u16,
    pub slot_index: u32,
}

pub(crate) async fn lookup_edge_equal_local(
    index: Option<&dyn PropertyIndexLookup>,
    property_id: PropertyId,
    expected: &[u8],
    catalog_label_id: Option<u16>,
) -> Result<Vec<LocalEdgePosting>, PlanQueryError> {
    if let Some(ix) = index {
        let physical_index_ids = match catalog_label_id {
            Some(catalog) => {
                crate::index::catalog_context::active_edge_physical_index_ids_for_catalog_label(
                    catalog,
                    property_id,
                )
            }
            None => crate::index::catalog_context::active_edge_physical_index_ids_for_property(
                property_id,
            ),
        };
        if physical_index_ids.is_empty() {
            return Err(PlanQueryError::UnsupportedOp(
                "EdgeIndex(no active physical index namespace)",
            ));
        }
        let mut hits: Vec<EdgePostingHit> = Vec::new();
        for physical_index_id in physical_index_ids {
            for hit in ix
                .lookup_edge_equal(
                    physical_index_id,
                    property_id.raw(),
                    expected.to_vec(),
                    catalog_label_id,
                )
                .await?
            {
                if !hits.contains(&hit) {
                    hits.push(hit);
                }
            }
        }
        let shard_id = ix.local_shard_id();
        return Ok(hits
            .into_iter()
            .filter(|hit| hit.shard_id == shard_id)
            .map(|hit| LocalEdgePosting {
                owner_vertex_id: VertexId::from(hit.owner_vertex_id),
                // Stored postings carry the wire tag; it binds directly to the CSR bucket.
                label_id: hit.label_id,
                slot_index: hit.slot_index,
            })
            .collect());
    }
    Ok(scan_store_edge_equal(
        property_id,
        expected,
        catalog_label_id,
    ))
}

pub(crate) fn lookup_edge_equal_local_sync(
    index: Option<&dyn PropertyIndexLookup>,
    property_id: PropertyId,
    expected: &[u8],
    catalog_label_id: Option<u16>,
) -> Result<Vec<LocalEdgePosting>, PlanQueryError> {
    if let Some(ix) = index {
        return pollster::block_on(lookup_edge_equal_local(
            Some(ix),
            property_id,
            expected,
            catalog_label_id,
        ));
    }
    Ok(scan_store_edge_equal(
        property_id,
        expected,
        catalog_label_id,
    ))
}

/// Union of point-probe postings over independently resolved equality payloads
/// (an `IN`-list anchor). Each payload probes once and postings deduplicate on
/// edge identity, so an edge matching duplicate list elements merges into one
/// candidate.
pub(crate) fn lookup_edge_equal_union_local(
    index: Option<&dyn PropertyIndexLookup>,
    property_id: PropertyId,
    expected_payloads: &[Vec<u8>],
    catalog_label_id: Option<u16>,
) -> Result<Vec<LocalEdgePosting>, PlanQueryError> {
    let mut merged: Vec<LocalEdgePosting> = Vec::new();
    let mut seen = std::collections::BTreeSet::<(u32, u16, u32)>::new();
    for expected in expected_payloads {
        for posting in lookup_edge_equal_local_sync(index, property_id, expected, catalog_label_id)?
        {
            if seen.insert((
                u32::from(posting.owner_vertex_id),
                posting.label_id,
                posting.slot_index,
            )) {
                merged.push(posting);
            }
        }
    }
    Ok(merged)
}
/// Shard-local ordered edge range over the domain-clamped `[low, high)` encoded interval.
///
/// Falls back to a canonical `EDGE_PROPERTIES` filtered scan when no graph-index client is
/// wired; the caller keeps the original predicate as a residual filter, so the fallback only
/// needs to produce a superset of the matching edges.
pub(crate) async fn lookup_edge_range_local(
    index: Option<&dyn PropertyIndexLookup>,
    property_id: PropertyId,
    low: &[u8],
    high: &[u8],
    catalog_label_id: Option<u16>,
) -> Result<Vec<LocalEdgePosting>, PlanQueryError> {
    if let Some(ix) = index {
        let physical_index_ids = match catalog_label_id {
            Some(catalog) => {
                crate::index::catalog_context::active_edge_physical_index_ids_for_catalog_label(
                    catalog,
                    property_id,
                )
            }
            None => crate::index::catalog_context::active_edge_physical_index_ids_for_property(
                property_id,
            ),
        };
        if physical_index_ids.is_empty() {
            return Err(PlanQueryError::UnsupportedOp(
                "EdgeIndex(no active physical index namespace)",
            ));
        }
        let request = PostingRangeRequest::Between {
            low: low.to_vec(),
            high: high.to_vec(),
        };
        let mut hits: Vec<EdgePostingHit> = Vec::new();
        for physical_index_id in physical_index_ids {
            for hit in ix
                .lookup_edge_range(
                    physical_index_id,
                    property_id.raw(),
                    &request,
                    catalog_label_id,
                )
                .await?
            {
                if !hits.contains(&hit) {
                    hits.push(hit);
                }
            }
        }
        let shard_id = ix.local_shard_id();
        return Ok(hits
            .into_iter()
            .filter(|hit| hit.shard_id == shard_id)
            .map(|hit| LocalEdgePosting {
                owner_vertex_id: VertexId::from(hit.owner_vertex_id),
                // Stored postings carry the wire tag; it binds directly to the CSR bucket.
                label_id: hit.label_id,
                slot_index: hit.slot_index,
            })
            .collect());
    }
    Ok(store_scan_matching_edges(catalog_label_id, |label| {
        GraphStore::collect_edges_matching_indexed_property_where(property_id, label, |bytes| {
            bytes >= low && bytes < high
        })
    }))
}

/// Superset candidate scan for literals whose comparison domain cannot form an ordered
/// interval: every local edge holding the property. The plan retains the original predicate
/// as a residual filter, so this only has to produce a superset of matching edges.
pub(crate) fn lookup_edge_range_fallback_local(
    property_id: PropertyId,
    catalog_label_id: Option<u16>,
) -> Vec<LocalEdgePosting> {
    store_scan_matching_edges(catalog_label_id, |label| {
        GraphStore::collect_edges_matching_indexed_property_where(property_id, label, |_| true)
    })
}

fn scan_store_edge_equal(
    property_id: PropertyId,
    expected: &[u8],
    catalog_label_id: Option<u16>,
) -> Vec<LocalEdgePosting> {
    store_scan_matching_edges(catalog_label_id, |label| {
        GraphStore::collect_edges_matching_indexed_property(property_id, expected, label)
    })
}

/// Runs one canonical-store scan per requested label packing and maps the hits to local
/// postings. Storage keys are wire-tagged, so a catalog label scans both of its packings;
/// the packings are disjoint, so no deduplication is needed.
fn store_scan_matching_edges(
    catalog_label_id: Option<u16>,
    scan: impl Fn(Option<u16>) -> Vec<(ic_stable_lara::VertexId, u16, u32)>,
) -> Vec<LocalEdgePosting> {
    let mut out = Vec::new();
    for label in store_scan_labels(catalog_label_id) {
        out.extend(
            scan(label)
                .into_iter()
                .map(|(owner_vertex_id, label_id, slot_index)| LocalEdgePosting {
                    owner_vertex_id,
                    label_id,
                    slot_index,
                }),
        );
    }
    out
}
