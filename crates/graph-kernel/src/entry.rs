pub mod edge;
pub mod label;
pub mod property;
pub mod vertex;
pub mod weight;

pub use edge::{Edge, EdgeMeta, VertexEdgeId};
pub use label::{INLINE_EDGE_LABEL_MAX, InlineEdgeLabelId, LabelId, VERTEX_LABEL_MIN};
pub use property::PropertyId;
pub use vertex::Vertex;
pub use weight::{
    EdgeWeightProfile, PreparedWeightDecoder, WeightDecodeError, WeightEncoding,
    WeightProfilePrepareError, decode_inline_weight,
};
