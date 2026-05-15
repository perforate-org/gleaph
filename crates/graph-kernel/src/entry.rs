pub mod compact_edge;
pub mod edge;
pub mod label;
pub mod property;
pub mod vertex;
pub mod vertex_ref;
pub mod weight;

pub use compact_edge::CompactEdge;
pub use edge::{Edge, EdgeMeta, VertexEdgeId};
pub use label::{EDGE_LABEL_CATALOG_MAX, EDGE_LABEL_UNDIRECTED_BIT, EdgeLabelId, VertexLabelId};
pub use property::PropertyId;
pub use vertex::Vertex;
pub use vertex_ref::VertexRef;
pub use weight::{
    EdgeWeightProfile, PreparedWeightDecoder, WeightDecodeError, WeightEncoding,
    WeightProfilePrepareError, decode_inline_weight,
};
