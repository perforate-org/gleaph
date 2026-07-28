pub mod constraint_name;
pub mod edge;
pub mod edge_inline_property;
pub mod embedding_name;
pub mod graph;
pub mod graph_type_id;
pub mod index_name;
pub mod label;
pub mod property;
pub mod property_entity;
pub mod remote_vertex_id;
pub mod vertex;
pub mod vertex_ref;

pub use constraint_name::{CONSTRAINT_NAME_CATALOG_MAX, ConstraintNameId};
pub use edge::{Edge, EdgeMeta, EdgeSlotIndex};
pub use edge_inline_property::{
    DecodedEdgeInlinePropertyBytes, EdgeInlinePropertyBytes, EdgeInlinePropertyEncoding,
    EdgeInlinePropertyProfile, EdgeInlinePropertyProfileError, MAX_EDGE_INLINE_PROPERTY_BYTES,
    PreparedEdgeInlinePropertyBytesDecoder, decode_edge_inline_property,
};

/// Edge topology plus inline property bytes used during mutation assembly.
///
/// This is not a read result; for read-boundary property attachment see
/// [`ic_stable_lara::labeled::graph::traverse::EdgeWithInlineProperty`].
#[derive(Clone, Debug, PartialEq)]
pub struct EdgeWithInlinePropertyBytes {
    pub edge: Edge,
    pub inline_property: EdgeInlinePropertyBytes,
}

impl EdgeWithInlinePropertyBytes {
    /// Build a topology-only entry with empty inline property bytes.
    #[inline]
    pub fn new(edge: Edge) -> Self {
        Self {
            edge,
            inline_property: EdgeInlinePropertyBytes::EMPTY,
        }
    }

    /// Build an entry with the given inline property bytes.
    #[inline]
    pub fn with_inline_property_bytes(edge: Edge, bytes: &[u8]) -> Self {
        Self {
            edge,
            inline_property: EdgeInlinePropertyBytes::from_slice(bytes),
        }
    }

    /// Returns the inline property bytes slice.
    #[inline]
    pub fn inline_property_bytes(&self) -> &[u8] {
        self.inline_property.as_slice()
    }

    /// Byte width of the stored inline property bytes.
    #[inline]
    pub fn inline_property_byte_width(&self) -> u16 {
        u16::try_from(self.inline_property.len()).unwrap_or(u16::MAX)
    }
}
pub use embedding_name::{EMBEDDING_NAME_CATALOG_MAX, EmbeddingNameId};
pub use graph::GraphId;
pub use graph_type_id::GraphTypeId;
pub use index_name::{INDEX_NAME_CATALOG_MAX, IndexNameId};
pub use label::{
    EDGE_LABEL_CATALOG_MAX, EDGE_LABEL_DIRECTED_BIT, EdgeDirectedness, EdgeLabelId,
    TaggedEdgeLabelId, VertexLabelId,
};
pub use property::PropertyId;
pub use property_entity::PropertyEntity;
pub use remote_vertex_id::{EdgeTarget, RemoteVertexId};
pub use vertex::Vertex;
pub use vertex_ref::VertexRef;
