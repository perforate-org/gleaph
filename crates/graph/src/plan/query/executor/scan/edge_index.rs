//! Leading-edge equality index scan and endpoint binding.

use std::collections::BTreeMap;

use gleaph_gql::Value;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{ScanValue, Str};
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::CsrEdge;

use crate::facade::catalog_edge_label_from_wire;
use crate::facade::{EdgeHandle, GraphStore};
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::edge_lookup;
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::{EdgeBinding, hop_aux_scalar};
use crate::plan::query::executor::expand::{
    ExpandDst, expand_dst_binding, expand_dst_matches_prebound_vertex,
};
use crate::plan::query::executor::{PlanBinding, resolve_scan_payload_bytes};
use crate::plan::query::row::PlanRow;

fn property_id_for_scan(
    execution: &GqlExecutionContext,
    property_name: &str,
) -> Result<u32, PlanQueryError> {
    execution
        .resolved_property_id(property_name)
        .map(|p| p.raw())
        .ok_or_else(|| PlanQueryError::MissingResolvedProperty {
            name: property_name.to_owned(),
        })
}

fn edge_binding_from_posting(
    store: &GraphStore,
    owner_vertex_id: ic_stable_lara::VertexId,
    label_id: u16,
    slot_index: u32,
) -> Result<Option<EdgeBinding>, PlanQueryError> {
    let handle = EdgeHandle {
        owner_vertex_id,
        label_id: LaraLabelId::from_raw(label_id),
        slot_index,
    };
    let Some(edge) = store.find_outgoing_edge_record(handle)? else {
        return Ok(None);
    };
    // ADR-0021 read gate: an edge-property index scan binds the edge directly
    // (no `ExpandDst::from_edge` chokepoint), so gate both endpoints here. Hide
    // the edge while either endpoint is a tombstoned vertex mid-purge.
    if crate::facade::vertex_hidden_by_pending_purge(owner_vertex_id)
        || crate::facade::vertex_hidden_by_pending_purge(edge.neighbor_vid())
    {
        return Ok(None);
    }
    Ok(Some(EdgeBinding::from_edge(handle, edge)))
}

fn expand_endpoints_for_direction(
    handle: EdgeHandle,
    edge: &Edge,
    direction: EdgeDirection,
) -> Result<Option<(ExpandDst, ExpandDst)>, PlanQueryError> {
    // `from_edge` gates the neighbor endpoint (ADR 0021); gate the owner endpoint
    // here too, so an edge bound before its owner entered the pending-purge set is
    // hidden once the purge is in flight.
    if crate::facade::vertex_hidden_by_pending_purge(handle.owner_vertex_id) {
        return Ok(None);
    }
    let neighbor = ExpandDst::from_edge(edge)?;
    let owner = ExpandDst::Local(handle.owner_vertex_id);
    let Some(neighbor) = neighbor else {
        return Ok(None);
    };
    Ok(Some(match direction {
        EdgeDirection::PointingRight | EdgeDirection::Undirected => (owner, neighbor),
        EdgeDirection::PointingLeft => (neighbor, owner),
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }))
}

fn edge_binding_matches_label(
    binding: &EdgeBinding,
    label: Option<EdgeLabelId>,
) -> Result<bool, PlanQueryError> {
    let Some(expected) = label else {
        return Ok(true);
    };
    let wire = binding.handle.label_id;
    let catalog = catalog_edge_label_from_wire(wire);
    Ok(catalog == Some(expected))
}

pub(crate) fn execute_edge_index_scan(
    store: &GraphStore,
    index: Option<&dyn PropertyIndexLookup>,
    execution: &GqlExecutionContext,
    rows: Vec<PlanRow>,
    variable: &Str,
    property: &Str,
    scan_value: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let property_id = gleaph_graph_kernel::entry::PropertyId::from_raw(property_id_for_scan(
        execution,
        property.as_ref(),
    )?);
    let Some(expected) = resolve_scan_payload_bytes(scan_value, parameters)? else {
        return Ok(Vec::new());
    };
    let postings = edge_lookup::lookup_edge_equal_local_sync(index, property_id, &expected, None)?;
    if postings.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for row in rows {
        for posting in &postings {
            let Some(edge_binding) = edge_binding_from_posting(
                store,
                posting.owner_vertex_id,
                posting.label_id,
                posting.slot_index,
            )?
            else {
                continue;
            };
            out.push(row.fork([(variable.as_ref(), PlanBinding::Edge(edge_binding))]));
        }
    }
    Ok(out)
}

pub(crate) fn execute_edge_bind_endpoints(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    rows: Vec<PlanRow>,
    edge: &Str,
    near: &Str,
    far: &Str,
    direction: EdgeDirection,
    label: Option<&str>,
    near_property_projection: Option<&[Str]>,
    far_property_projection: Option<&[Str]>,
    hop_aux_binding: Option<&Str>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = match label {
        Some(name) => execution
            .resolved_edge_label_id(name)
            .map(Some)
            .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                namespace: "edge",
                name: name.to_owned(),
            })?,
        None => None,
    };

    let mut out = Vec::new();
    for row in rows {
        let Some(PlanBinding::Edge(edge_binding)) = row.get(edge.as_ref()) else {
            return Err(PlanQueryError::MissingBinding {
                variable: edge.to_string(),
            });
        };
        if !edge_binding_matches_label(edge_binding, label_id)? {
            continue;
        }
        let handle = edge_binding.handle;
        let Some(edge_record) = store.find_outgoing_edge_record(handle)? else {
            continue;
        };
        let Some((near_dst, far_dst)) =
            expand_endpoints_for_direction(handle, &edge_record, direction)?
        else {
            continue;
        };
        if !expand_dst_matches_prebound_vertex(&row, far, far_dst) {
            continue;
        }
        let near_binding =
            expand_dst_binding(store, execution, near_dst, near_property_projection)?;
        let far_binding = expand_dst_binding(store, execution, far_dst, far_property_projection)?;
        let mut updates = vec![(near.as_ref(), near_binding), (far.as_ref(), far_binding)];
        if let Some(hop_key) = hop_aux_binding {
            updates.push((
                hop_key.as_ref(),
                PlanBinding::Value(hop_aux_scalar(edge_binding)),
            ));
        }
        out.push(row.fork(updates));
    }
    Ok(out)
}
