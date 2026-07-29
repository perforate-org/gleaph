//! Scalar inline edge values translated into ordinary property-index postings.

use crate::edge_inline_property_scalar_codec::decode_edge_inline_property_scalar;
use crate::facade::catalog_edge_label_from_wire;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, TaggedEdgeLabelId};
use gleaph_graph_kernel::plan_exec::{ResolvedInlineSchema, ResolvedInlineStructField};

use super::{PropertyValueChange, dispatch_property_index_ops};

/// Decode an indexed scalar inline value from the canonical edge bytes.
/// Decode all indexed inline values on an edge. Struct leaves are keyed by their Router-interned
/// dotted property id, so no second schema or property-index identity is needed in Graph.
pub(crate) fn inline_index_values(
    wire_label_id: u16,
    inline_property_bytes: &[u8],
) -> Result<Vec<(PropertyId, Value)>, String> {
    let wire_label = TaggedEdgeLabelId::from_raw(wire_label_id);
    let Some(catalog_label) = catalog_edge_label_from_wire(wire_label) else {
        return Ok(Vec::new());
    };
    let Some(resolved) =
        crate::edge_inline_property_schema::resolved_edge_label_with(None, catalog_label)
    else {
        return Ok(Vec::new());
    };
    let Some(schema) = resolved.inline_schema.as_ref() else {
        return Ok(Vec::new());
    };
    let inline_id = schema.property_id();
    let memberships =
        crate::index::catalog_context::indexed_edge_memberships(wire_label_id, inline_id);
    if memberships.is_empty() {
        return Ok(Vec::new());
    }
    match schema {
        ResolvedInlineSchema::Scalar { .. } => decode_edge_inline_property_scalar(
            &resolved.inline_property_profile,
            inline_property_bytes,
        )
        .map(|value| {
            memberships
                .into_iter()
                .map(|(id, _)| (id, value.clone()))
                .collect()
        })
        .map_err(|err| format!("indexed inline scalar decode failed: {err}")),
        ResolvedInlineSchema::Struct { fields, .. } => memberships
            .into_iter()
            .filter_map(|(property_id, path)| {
                fields
                    .iter()
                    .find(|field| field.name == path)
                    .map(|field| (property_id, field))
            })
            .map(|(property_id, field)| {
                decode_struct_field(inline_property_bytes, field).map(|v| (property_id, v))
            })
            .collect(),
    }
}

fn decode_struct_field(bytes: &[u8], field: &ResolvedInlineStructField) -> Result<Value, String> {
    let start = usize::from(field.byte_offset);
    let width = usize::from(field.profile.required_byte_width());
    let end = start
        .checked_add(width)
        .ok_or_else(|| "inline struct field offset overflow".to_owned())?;
    let slice = bytes
        .get(start..end)
        .ok_or_else(|| "inline struct field lies outside inline property bytes".to_owned())?;
    decode_edge_inline_property_scalar(&field.profile, slice)
        .map_err(|err| format!("indexed inline struct field decode failed: {err}"))
}

pub(crate) fn dispatch_inline_index_removals(
    owner_vertex_id: ic_stable_lara::VertexId,
    wire_label_id: u16,
    slot_index: u32,
    inline_property_bytes: &[u8],
) -> Result<(), String> {
    for (property_id, value) in inline_index_values(wire_label_id, inline_property_bytes)? {
        dispatch_property_index_ops(PropertyValueChange::edge(
            owner_vertex_id,
            wire_label_id,
            slot_index,
            property_id,
            Some(&value),
            None,
        ));
    }
    Ok(())
}
