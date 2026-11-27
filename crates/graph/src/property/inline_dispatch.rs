//! Scalar inline edge values translated into ordinary property-index postings.

use crate::edge_inline_property_scalar_codec::decode_edge_inline_property_scalar;
use crate::facade::catalog_edge_label_from_wire;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, TaggedEdgeLabelId};
use gleaph_graph_kernel::plan_exec::{
    ResolvedEdgeLabel, ResolvedInlineSchema, ResolvedInlineStructField,
};

use crate::index::catalog_context::IndexMembershipRef;

use super::{PropertyValueChange, dispatch_property_index_ops_for_physical};

/// The indexed inline values of one edge label, resolved once: its inline schema and the index
/// memberships of its inline property. A caller with many edges of one label (a compaction
/// step's moves) resolves it once and decodes each edge's bytes with [`Self::values`].
pub(crate) struct InlineIndexing {
    label: ResolvedEdgeLabel,
    memberships: Vec<(IndexMembershipRef, PropertyId, String)>,
}

/// One indexed inline value: the membership, the property and the value.
pub(crate) type IndexedInlineValue = (IndexMembershipRef, PropertyId, Value);

impl InlineIndexing {
    /// `wire_label_id`'s inline indexing; `None` when no index takes its inline values.
    pub(crate) fn of(wire_label_id: u16) -> Option<Self> {
        let catalog_label =
            catalog_edge_label_from_wire(TaggedEdgeLabelId::from_raw(wire_label_id))?;
        let label =
            crate::edge_inline_property_schema::resolved_edge_label_with(None, catalog_label)?;
        let inline_id = label.inline_schema.as_ref()?.property_id();
        let memberships =
            crate::index::catalog_context::indexed_edge_memberships(wire_label_id, inline_id);
        (!memberships.is_empty()).then_some(Self { label, memberships })
    }

    /// The indexed values in one edge's inline bytes.
    pub(crate) fn values(
        &self,
        inline_property_bytes: &[u8],
    ) -> Result<Vec<IndexedInlineValue>, String> {
        match self.label.inline_schema.as_ref() {
            None => Ok(Vec::new()),
            Some(ResolvedInlineSchema::Scalar { .. }) => decode_edge_inline_property_scalar(
                &self.label.inline_property_profile,
                inline_property_bytes,
            )
            .map(|value| {
                self.memberships
                    .iter()
                    .map(|&(membership, id, _)| (membership, id, value.clone()))
                    .collect()
            })
            .map_err(|err| format!("indexed inline scalar decode failed: {err}")),
            Some(ResolvedInlineSchema::Struct { fields, .. }) => self
                .memberships
                .iter()
                .filter_map(|(membership, property_id, path)| {
                    fields
                        .iter()
                        .find(|field| &field.name == path)
                        .map(|field| (*membership, *property_id, field))
                })
                .map(|(membership, property_id, field)| {
                    decode_struct_field(inline_property_bytes, field)
                        .map(|v| (membership, property_id, v))
                })
                .collect(),
        }
    }
}

/// Decode all indexed inline values on an edge. Struct leaves are keyed by their Router-interned
/// dotted property id, so no second schema or property-index identity is needed in Graph.
pub(crate) fn inline_index_values(
    wire_label_id: u16,
    inline_property_bytes: &[u8],
) -> Result<Vec<IndexedInlineValue>, String> {
    InlineIndexing::of(wire_label_id).map_or_else(
        || Ok(Vec::new()),
        |indexing| indexing.values(inline_property_bytes),
    )
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
    owner_vertex_id: gleaph_graph_kernel::entry::VertexId,
    wire_label_id: u16,
    slot_index: u32,
    inline_property_bytes: &[u8],
) -> Result<(), String> {
    for (membership, property_id, value) in
        inline_index_values(wire_label_id, inline_property_bytes)?
    {
        dispatch_property_index_ops_for_physical(
            PropertyValueChange::edge(
                owner_vertex_id,
                wire_label_id,
                slot_index,
                property_id,
                Some(&value),
                None,
            ),
            membership,
        );
    }
    Ok(())
}
