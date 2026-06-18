//! Labeled graph errors.

use crate::{
    VertexCount, VertexId,
    labeled::record::LabeledVertexFieldError,
    lara::{
        edge::InitError as EdgeInitError,
        edge_payload::{InitError as ValueInitError, PayloadLogReadError, PayloadLogWriteError},
        operation_error::LaraOperationError,
        vertex::InitError as VertexInitError,
    },
};
use std::fmt;

/// Errors returned by labeled graph operations.
#[derive(Debug)]
pub enum LabeledOperationError {
    /// Addressing a vertex outside `0..vertex_count`.
    VertexOutOfRange {
        /// Requested vertex id.
        vid: VertexId,
        /// Current vertex column length.
        len: VertexCount,
    },
    /// Underlying LARA store operation failed.
    Store(LaraOperationError),
    /// Reading an edge-payload overflow-log entry failed.
    PayloadLogRead(PayloadLogReadError),
    /// Writing an edge-payload overflow-log entry failed.
    PayloadLogWrite(PayloadLogWriteError),
    /// A default-label bypass was requested for a row that cannot use it.
    InvalidDefaultBypass,
    /// An edge payload byte width did not match the label bucket payload schema.
    PayloadByteWidthMismatch {
        /// Payload byte width declared by the label bucket.
        bucket_width: u16,
        /// Payload byte width carried by the edge.
        edge_payload_width: u16,
    },
    /// Vertex row fields are inconsistent with labeled bucket-mode limits.
    InvalidVertexRow(LabeledVertexFieldError),
}

impl fmt::Display for LabeledOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
            Self::Store(err) => write!(f, "{err}"),
            Self::PayloadLogRead(err) => write!(f, "{err}"),
            Self::PayloadLogWrite(err) => write!(f, "{err}"),
            Self::InvalidDefaultBypass => write!(
                f,
                "default-label bypass requires exactly one default adjacency label"
            ),
            Self::PayloadByteWidthMismatch {
                bucket_width,
                edge_payload_width,
            } => write!(
                f,
                "edge payload byte width {edge_payload_width} does not match label bucket payload byte width {bucket_width}"
            ),
            Self::InvalidVertexRow(err) => write!(f, "invalid labeled vertex row: {err:?}"),
        }
    }
}

impl std::error::Error for LabeledOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::PayloadLogRead(err) => Some(err),
            Self::PayloadLogWrite(err) => Some(err),
            Self::VertexOutOfRange { .. }
            | Self::InvalidDefaultBypass
            | Self::PayloadByteWidthMismatch { .. }
            | Self::InvalidVertexRow(_) => None,
        }
    }
}

impl From<LabeledVertexFieldError> for LabeledOperationError {
    fn from(err: LabeledVertexFieldError) -> Self {
        Self::InvalidVertexRow(err)
    }
}

impl From<LabeledVertexFieldError> for LaraOperationError {
    fn from(err: LabeledVertexFieldError) -> Self {
        match err {
            LabeledVertexFieldError::LabelBucketCountOverflow
            | LabeledVertexFieldError::LabelBucketDescriptorSpanOverflow => Self::RowDegreeOverflow,
            LabeledVertexFieldError::SlotIndexOverflow
            | LabeledVertexFieldError::MetadataReservedBitSet
            | LabeledVertexFieldError::BypassOverflowLogHeadOutOfRange
            | LabeledVertexFieldError::ValueAllocatedBytesOverflow => {
                Self::CollectAllocationOverflow
            }
        }
    }
}

impl From<crate::labeled::record::LabelBucketFieldError> for LabeledOperationError {
    fn from(err: crate::labeled::record::LabelBucketFieldError) -> Self {
        Self::Store(err.into())
    }
}

impl From<crate::labeled::record::LabelBucketFieldError> for LaraOperationError {
    fn from(err: crate::labeled::record::LabelBucketFieldError) -> Self {
        match err {
            crate::labeled::record::LabelBucketFieldError::SlotIndexOverflow => {
                Self::CollectAllocationOverflow
            }
            crate::labeled::record::LabelBucketFieldError::ReservedTopBitSet
            | crate::labeled::record::LabelBucketFieldError::OverflowLogHeadOutOfRange
            | crate::labeled::record::LabelBucketFieldError::PayloadOffsetOverflow
            | crate::labeled::record::LabelBucketFieldError::PayloadLogHeadOutOfRange
            | crate::labeled::record::LabelBucketFieldError::PayloadLogLenOutOfRange
            | crate::labeled::record::LabelBucketFieldError::PayloadLogStateMismatch => {
                Self::CollectAllocationOverflow
            }
        }
    }
}

impl From<LaraOperationError> for LabeledOperationError {
    fn from(value: LaraOperationError) -> Self {
        Self::Store(value)
    }
}

impl From<crate::GrowFailed> for LabeledOperationError {
    fn from(value: crate::GrowFailed) -> Self {
        Self::Store(LaraOperationError::RebalanceFailed(value))
    }
}

impl From<PayloadLogReadError> for LabeledOperationError {
    fn from(value: PayloadLogReadError) -> Self {
        Self::PayloadLogRead(value)
    }
}

impl From<PayloadLogWriteError> for LabeledOperationError {
    fn from(value: PayloadLogWriteError) -> Self {
        Self::PayloadLogWrite(value)
    }
}

/// Errors returned when reopening a labeled graph.
#[derive(Debug)]
pub enum InitError {
    /// The vertex column could not be reopened.
    Vertices(VertexInitError),
    /// The label-bucket subsystem could not be reopened.
    Buckets(crate::labeled::LabelBucketStoreInitError),
    /// The edge subsystem could not be reopened.
    Edges(EdgeInitError),
    /// The edge-payload byte slab could not be reopened.
    Payloads(ValueInitError),
    /// The graph-owned memories are partially initialized (some regions are empty
    /// while others are populated), so the graph must not be reopened or recreated.
    PartialLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vertices(e) => write!(f, "vertex init failed: {e}"),
            Self::Buckets(e) => write!(f, "bucket init failed: {e}"),
            Self::Edges(e) => write!(f, "edge init failed: {e}"),
            Self::Payloads(e) => write!(f, "payload slab init failed: {e}"),
            Self::PartialLayout => {
                write!(
                    f,
                    "graph memories are partially initialized; refusing to reopen"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Outgoing-edge scan order for APIs that expose both the hot descending walk and the stable
/// ascending materialization order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OutEdgeOrder {
    /// Default hot-path order: label buckets high→low; within each span, overflow log head first
    /// and then slab slots high→low.
    #[default]
    Descending,
    /// Stable materialization order: label buckets low→high; within each span, CSR slots low→high.
    Ascending,
}

impl OutEdgeOrder {
    pub(super) fn ascending(self) -> bool {
        matches!(self, Self::Ascending)
    }
}
