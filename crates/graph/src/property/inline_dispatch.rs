//! Scalar inline edge values translated into ordinary property-index postings.

use crate::edge_inline_property_scalar_codec::decode_edge_inline_property_scalar;
use crate::facade::catalog_edge_label_from_wire;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, TaggedEdgeLabelId};
use gleaph_graph_kernel::plan_exec::ResolvedInlineSchema;

use super::{PropertyValueChange, dispatch_property_index_ops};

/// Decode an indexed scalar inline value from the canonical edge bytes.
pub(crate) fn inline_scalar_index_value(
    wire_label_id: u16,
    inline_property_bytes: &[u8],
) -> Result<Option<(PropertyId, Value)>, String> {
    let wire_label = TaggedEdgeLabelId::from_raw(wire_label_id);
    let Some(catalog_label) = catalog_edge_label_from_wire(wire_label) else {
        return Ok(None);
    };
    let Some(resolved) =
        crate::edge_inline_property_schema::resolved_edge_label_with(None, catalog_label)
    else {
        return Ok(None);
    };
    let Some(ResolvedInlineSchema::Scalar { property_id }) = resolved.inline_schema else {
        return Ok(None);
    };
    if !crate::index::catalog_context::should_maintain_edge_posting(wire_label_id, property_id) {
        return Ok(None);
    }
    decode_edge_inline_property_scalar(&resolved.inline_property_profile, inline_property_bytes)
        .map(|value| Some((property_id, value)))
        .map_err(|err| format!("indexed inline scalar decode failed: {err}"))
}

pub(crate) fn dispatch_inline_scalar_index_removal(
    owner_vertex_id: ic_stable_lara::VertexId,
    wire_label_id: u16,
    slot_index: u32,
    inline_property_bytes: &[u8],
) -> Result<(), String> {
    let Some((property_id, value)) =
        inline_scalar_index_value(wire_label_id, inline_property_bytes)?
    else {
        return Ok(());
    };
    dispatch_property_index_ops(PropertyValueChange::edge(
        owner_vertex_id,
        wire_label_id,
        slot_index,
        property_id,
        Some(&value),
        None,
    ));
    Ok(())
}
