//! Labeled graph errors.

use crate::{
    VertexCount, VertexId,
    labeled::{
        bucket_label_key::BucketLabelKey, ltb_raw_block_store,
        ltb_raw_block_store::BlockError as LtbBlockError, record::LabeledVertexFieldError,
    },
    lara::{
        edge::InitError as EdgeInitError,
        edge_inline_property::{
            InitError as ValueInitError, InlinePropertyBytesLogReadError,
            InlinePropertyBytesLogWriteError,
        },
        operation_error::LaraOperationError,
        vertex::InitError as VertexInitError,
    },
};
use std::fmt;

pub(crate) use super::BucketMode;

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
    /// Reading an edge-inline-property-bytes overflow-log entry failed.
    InlinePropertyBytesLogRead(InlinePropertyBytesLogReadError),
    /// Writing an edge-inline-property-bytes overflow-log entry failed.
    InlinePropertyBytesLogWrite(InlinePropertyBytesLogWriteError),
    /// A default-label bypass was requested for a row that cannot use it.
    InvalidDefaultBypass,
    /// An edge inline property byte width did not match the label bucket inline property schema.
    InlinePropertyBytesWidthMismatch {
        /// Payload byte width declared by the label bucket.
        bucket_width: u16,
        /// Payload byte width carried by the edge.
        edge_inline_property_width: u16,
    },
    /// Vertex row fields are inconsistent with labeled bucket-mode limits.
    InvalidVertexRow(LabeledVertexFieldError),
    /// `alloc_space = stored_slots + alloc_gap` reached the mode cap. The caller
    /// must trigger `promote_bypass_to_tree_mode` (slab mode) or `deepen` (tree
    /// mode) before retrying. Plan 0318 §Step 3 cap enforcement.
    AllocSpaceCapReached {
        /// Current `alloc_space` (capped at the mode cap).
        current_alloc_space: u32,
        /// Mode cap (`T_PROMOTE` for slab, `R_MAX` for tree).
        cap: u32,
        /// Bucket mode at the time the cap was hit.
        mode: BucketMode,
    },
    /// Vertex already holds `MAX_BUCKETS_PER_VERTEX` distinct edge-label-type
    /// buckets. Caller must re-classify this vertex (split, federate, or fail).
    /// Plan 0318 §Step 3 cap enforcement.
    VertexBucketCountCapReached {
        /// Current distinct-bucket count for the vertex.
        current_count: u32,
        /// `MAX_BUCKETS_PER_VERTEX`.
        cap: u32,
    },
    /// Tree-mode LTB store reported a [`LtbBlockError`] (e.g. `NotMinted`,
    /// `OutOfRange`, `OutOfBounds`). Surfaces invariant violations in the
    /// promote path; the promotion fails closed and the bucket remains in
    /// its pre-promotion state. Plan 0318 §Step 4.
    LtbBlock(LtbBlockError),
    /// No labeled bucket exists for the requested `(vid, label)` pair, so
    /// the promote path cannot proceed. Caller is expected to insert an
    /// edge (which creates the bucket) before retrying the promotion. Plan
    /// 0318 §Step 4 amend (replaces the misleading
    /// `AllocSpaceCapReached { current_alloc_space: 0, cap: T_PROMOTE, mode: Slab }`
    /// that the original Step 4 implementation surfaced for the `Missing`
    /// branch).
    BucketNotFound {
        /// Requested vertex id.
        vid: VertexId,
        /// Requested label key.
        label: BucketLabelKey,
    },
    /// Tree-mode edge append was attempted for an `E` whose wire width is
    /// not 4 bytes. Tree-mode LTB blocks are 4-byte-aligned (each block
    /// holds `B = 1024` 4-byte targets); wider edges would break the
    /// block math. Plan 0318 §Step 6 typed guard (replaces the Step 5
    /// `debug_assert_eq!(E::BYTES, 4)` at every tree read entry).
    TreeModeEdgeWidthUnsupported {
        /// `E::BYTES` reported by the edge type.
        actual: usize,
        /// Required width for tree-mode LTB block alignment (4).
        expected: usize,
    },
    /// Tree-mode `deepen` was requested but the bucket is already at the
    /// structural maximum depth (`MAX_DEPTH = 3`, per ADR 0088 §4). The
    /// fan-out of every interior level is `K = R_MAX = 1024`; once a
    /// depth-3 root is full, the bucket cannot be packed further. This
    /// is the typed guard that replaces the prototype's
    /// `derive_depth` panic on the production wire-up.
    TreeDepthLimitReached {
        /// Depth the bucket was already at when `deepen` was called.
        depth: u32,
        /// Structural maximum depth (currently 3, per ADR 0088 §4).
        max_depth: u32,
    },
    /// Plan 0318 §Step 7 amend (interim): tree-mode `insert` would
    /// push the physical root region past `R_MAX = 1024` entries. The
    /// interior-level insert cascade (right-spine growth at depth
    /// ≥ 2) is not yet wired, so a depth-1 bucket at root_len = R_MAX
    /// (= 1024) cannot accept a 1,048,577th slot. Until the cascade
    /// is implemented (follow-up todo), the production insert path
    /// fails closed at `stored = 2^20 = 1,048,576` slots (= 4 MiB of
    /// edge data) with this typed error instead of silently growing
    /// the root past the ADR wire truth (`root_len ≤ R_max`, ADR 0088
    /// §4).
    TreeRootCapacityReached {
        /// The `next_stored` value the insert would have produced.
        stored_slots: u32,
        /// Physical root region length at the time of the failed insert.
        root_len: u32,
        /// Structural cap (`R_MAX = 1024` per ADR 0088 §4).
        cap: u32,
    },
}

impl fmt::Display for LabeledOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::VertexOutOfRange { vid, len } => {
                write!(f, "vertex {vid} out of range (len={len})")
            }
            Self::Store(err) => write!(f, "{err}"),
            Self::InlinePropertyBytesLogRead(err) => write!(f, "{err}"),
            Self::InlinePropertyBytesLogWrite(err) => write!(f, "{err}"),
            Self::InvalidDefaultBypass => write!(
                f,
                "default-label bypass requires exactly one default adjacency label"
            ),
            Self::InlinePropertyBytesWidthMismatch {
                bucket_width,
                edge_inline_property_width,
            } => write!(
                f,
                "edge inline property byte width {edge_inline_property_width} does not match label bucket inline property byte width {bucket_width}"
            ),
            Self::InvalidVertexRow(err) => write!(f, "invalid labeled vertex row: {err:?}"),
            Self::AllocSpaceCapReached {
                current_alloc_space,
                cap,
                mode,
            } => write!(
                f,
                "labeled bucket alloc_space cap reached: current={current_alloc_space}, cap={cap}, mode={mode:?}"
            ),
            Self::VertexBucketCountCapReached { current_count, cap } => write!(
                f,
                "vertex bucket count cap reached: current_count={current_count}, cap={cap}"
            ),
            Self::LtbBlock(err) => write!(f, "ltb block error: {err:?}"),
            Self::BucketNotFound { vid, label } => {
                write!(
                    f,
                    "labeled bucket not found for promotion: vid={vid}, label={label:?}"
                )
            }
            Self::TreeModeEdgeWidthUnsupported { actual, expected } => write!(
                f,
                "tree-mode edge append requires E::BYTES == {expected} (got {actual})"
            ),
            Self::TreeDepthLimitReached { depth, max_depth } => write!(
                f,
                "tree-mode depth limit reached: depth={depth}, max_depth={max_depth}"
            ),
            Self::TreeRootCapacityReached {
                stored_slots,
                root_len,
                cap,
            } => write!(
                f,
                "tree-mode root region at capacity: root_len={root_len}, cap={cap}, \
                 stored_slots={stored_slots} (interior-level insert cascade is not yet \
                 wired; follow-up todo: tree-mode-interior-level-insert-growth)"
            ),
        }
    }
}

impl std::error::Error for LabeledOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::InlinePropertyBytesLogRead(err) => Some(err),
            Self::InlinePropertyBytesLogWrite(err) => Some(err),
            Self::VertexOutOfRange { .. }
            | Self::InvalidDefaultBypass
            | Self::InlinePropertyBytesWidthMismatch { .. }
            | Self::InvalidVertexRow(_)
            | Self::AllocSpaceCapReached { .. }
            | Self::VertexBucketCountCapReached { .. }
            | Self::LtbBlock(_)
            | Self::BucketNotFound { .. }
            | Self::TreeModeEdgeWidthUnsupported { .. }
            | Self::TreeDepthLimitReached { .. }
            | Self::TreeRootCapacityReached { .. } => None,
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
            crate::labeled::record::LabelBucketFieldError::ReservedBitsSet
            | crate::labeled::record::LabelBucketFieldError::OverflowLogHeadOutOfRange
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesOffsetOverflow
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesLogHeadOutOfRange
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesLogStateMismatch
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesStateWithoutSchema => {
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

impl From<InlinePropertyBytesLogReadError> for LabeledOperationError {
    fn from(value: InlinePropertyBytesLogReadError) -> Self {
        Self::InlinePropertyBytesLogRead(value)
    }
}

impl From<InlinePropertyBytesLogWriteError> for LabeledOperationError {
    fn from(value: InlinePropertyBytesLogWriteError) -> Self {
        Self::InlinePropertyBytesLogWrite(value)
    }
}

impl From<LtbBlockError> for LabeledOperationError {
    /// Bridge LTB `BlockError` to `LabeledOperationError::LtbBlock` so the
    /// promote path's `?` operator works across the LTB store. The
    /// `BlockError` variants (`NotMinted`, `OutOfRange`, `OutOfBounds`) are
    /// programming-error surfaces in normal use; they only fire when the
    /// caller drives the LTB store out of contract.
    fn from(value: LtbBlockError) -> Self {
        Self::LtbBlock(value)
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
    /// The edge-inline-property-bytes byte slab could not be reopened.
    InlinePropertyBytes(ValueInitError),
    /// The LARA Tree Block (LTB) store could not be reopened. Per Plan 0318 §Step 2,
    /// the LTB is lazily created; an empty LTB reopens via the asymmetric rule
    /// (like `value_blobs`), but a populated LTB must have valid magic "LTB",
    /// version 1, payload_bytes = 4096, R_max = 1024, and a consistent free list.
    Ltb(ltb_raw_block_store::InitError),
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
            Self::InlinePropertyBytes(e) => {
                write!(f, "inline property bytes slab init failed: {e}")
            }
            Self::Ltb(e) => write!(f, "ltb store init failed: {e}"),
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

/// Outgoing-edge scan order for APIs that expose both the stable ascending materialization order
/// and the explicit hot descending walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OutEdgeOrder {
    /// Default stable materialization order: label buckets low→high; within each span, CSR slots
    /// low→high.
    #[default]
    Ascending,
    /// Explicit hot-path order: label buckets high→low; within each span, overflow log head first
    /// and then slab slots high→low.
    Descending,
}

impl OutEdgeOrder {
    pub(super) fn ascending(self) -> bool {
        matches!(self, Self::Ascending)
    }
}
