pub mod edge;
pub mod edge_payload;
pub mod label;
pub mod property;
pub mod remote_ref;
pub mod vertex;
pub mod vertex_ref;
pub mod weight;

pub use edge::{Edge, EdgeMeta, EdgeSlotIndex};
pub use edge_payload::{
    DecodedEdgePayload, EdgePayload, EdgePayloadEncoding, EdgePayloadProfile,
    EdgePayloadProfileError, MAX_EDGE_PAYLOAD_BYTES, PreparedEdgePayloadDecoder,
    decode_edge_payload, decode_edge_weight,
};
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
    WeightProfilePrepareError,
};
