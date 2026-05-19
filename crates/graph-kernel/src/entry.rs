pub mod edge;
pub mod label;
pub mod property;
pub mod remote_ref;
pub mod vertex;
pub mod vertex_ref;
pub mod weight;

pub use edge::{Edge, EdgeMeta, EdgeSlotIndex};
pub use label::{
    EDGE_LABEL_CATALOG_MAX, EDGE_LABEL_DIRECTED_BIT, EdgeDirectedness, EdgeLabelId,
    TaggedEdgeLabelId, VertexLabelId,
};
pub use property::PropertyId;
pub use remote_ref::{EdgeTarget, RemoteRefId};
pub use vertex::Vertex;
pub use vertex_ref::VertexRef;
pub use weight::{
    EdgeWeightProfile, PreparedWeightDecoder, WeightDecodeError, WeightEncoding,
    WeightProfilePrepareError, decode_inline_weight,
};
