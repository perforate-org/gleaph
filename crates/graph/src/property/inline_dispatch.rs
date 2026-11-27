//! Scalar inline edge values translated into ordinary property-index postings.

use crate::edge_inline_property_scalar_codec::ScalarDecoder;
use crate::facade::catalog_edge_label_from_wire;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, TaggedEdgeLabelId};
use gleaph_graph_kernel::plan_exec::{ResolvedInlineSchema, ResolvedInlineStructField};

use crate::index::catalog_context::IndexMembershipRef;

use super::{PropertyValueChange, dispatch_property_index_ops_for_physical};

/// The indexed inline values of one edge label, resolved once: its inline schema, the index
/// memberships of its inline property, and a decoder prepared for each. A caller with many
/// edges of one label (a compaction step's moves) resolves it once and decodes each edge's
/// bytes with [`Self::values`].
pub(crate) struct InlineIndexing(InlineReads);

/// How an edge's inline bytes yield the indexed values.
enum InlineReads {
    /// One scalar, the whole inline value, indexed by every membership.
    Scalar {
        decoder: ScalarDecoder,
        memberships: Vec<(IndexMembershipRef, PropertyId)>,
    },
    /// Struct fields, one a membership: a membership whose field the struct lacks reads none.
    Struct(Vec<FieldRead>),
}

/// One membership's struct field: where it lies in the inline bytes and its decoder.
struct FieldRead {
    membership: IndexMembershipRef,
    property_id: PropertyId,
    start: usize,
    width: usize,
    decoder: ScalarDecoder,
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
        let schema = label.inline_schema.as_ref()?;
        let memberships = crate::index::catalog_context::indexed_edge_memberships(
            wire_label_id,
            schema.property_id(),
        );
        if memberships.is_empty() {
            return None;
        }
        let reads = match schema {
            ResolvedInlineSchema::Scalar { .. } => InlineReads::Scalar {
                decoder: ScalarDecoder::new(&label.inline_property_profile),
                memberships: memberships
                    .into_iter()
                    .map(|(membership, property_id, _)| (membership, property_id))
                    .collect(),
            },
            ResolvedInlineSchema::Struct { fields, .. } => InlineReads::Struct(
                memberships
                    .into_iter()
                    .filter_map(|(membership, property_id, path)| {
                        let field = fields.iter().find(|field| field.name == path)?;
                        Some(FieldRead::new(membership, property_id, field))
                    })
                    .collect(),
            ),
        };
        Some(Self(reads))
    }

    /// The indexed values in one edge's inline bytes.
    pub(crate) fn values(
        &self,
        inline_property_bytes: &[u8],
    ) -> Result<Vec<IndexedInlineValue>, String> {
        match &self.0 {
            InlineReads::Scalar {
                decoder,
                memberships,
            } => decoder
                .decode(inline_property_bytes)
                .map(|value| {
                    memberships
                        .iter()
                        .map(|&(membership, id)| (membership, id, value.clone()))
                        .collect()
                })
                .map_err(|err| format!("indexed inline scalar decode failed: {err}")),
            InlineReads::Struct(fields) => fields
                .iter()
                .map(|field| {
                    field
                        .decode(inline_property_bytes)
                        .map(|value| (field.membership, field.property_id, value))
                })
                .collect(),
        }
    }
}

impl FieldRead {
    fn new(
        membership: IndexMembershipRef,
        property_id: PropertyId,
        field: &ResolvedInlineStructField,
    ) -> Self {
        Self {
            membership,
            property_id,
            start: usize::from(field.byte_offset),
            width: usize::from(field.profile.required_byte_width()),
            decoder: ScalarDecoder::new(&field.profile),
        }
    }

    fn decode(&self, bytes: &[u8]) -> Result<Value, String> {
        let end = self
            .start
            .checked_add(self.width)
            .ok_or_else(|| "inline struct field offset overflow".to_owned())?;
        let slice = bytes
            .get(self.start..end)
            .ok_or_else(|| "inline struct field lies outside inline property bytes".to_owned())?;
        self.decoder
            .decode(slice)
            .map_err(|err| format!("indexed inline struct field decode failed: {err}"))
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
