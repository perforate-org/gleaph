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
    /// A physical edge/property slot is outside the bucket's stored extent.
    EdgeSlotOutOfRange {
        /// Requested physical slot.
        slot: u32,
        /// Bucket's exclusive physical-slot bound.
        stored_slots: u32,
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
    /// A terminal materialization cursor cannot be used again.
    InlinePropertyMaterializationConsumed,
    /// The admitted bucket is no longer a live, canonical source for this cursor.
    InlinePropertyMaterializationInvalidSource,
    /// Vertex row fields are inconsistent with labeled bucket-mode limits.
    InvalidVertexRow(LabeledVertexFieldError),
    /// The bucket's resident rows reached the mode cap. The caller
    /// must trigger `promote_bucket_to_tree_mode` (slab mode) or `deepen` (tree
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
    /// Tree mode stores one edge record per LTB slot (`E::BYTES` bytes) and its block
    /// ids in edge-record slots, so its geometry only closes when the record is exactly
    /// `PROPERTY_ID_BYTES` = 4 bytes wide. Any other width is refused at promotion
    /// instead of building a tree whose block payload, id grid and fan-out disagree
    /// (ADR 0088 §2).
    TreeModeEdgeWidthUnsupported {
        /// `E::BYTES` of the edge type the graph was opened with.
        edge_bytes: usize,
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
    /// The leaf's extent cannot take the request: no free run in the vertex's slice, no
    /// room to grow the extent, and relocation could not make one. A resident is never
    /// allocated outside the leaf (ADR 0097 I5), so this is the fail-closed answer rather
    /// than a fall back to the global free-span store.
    LeafArenaExhausted {
        /// The vertex whose region could not be placed.
        vid: VertexId,
        /// Slots the request needed.
        need: u64,
    },
    /// Plan 0326 LPB-in-tree (F-2 cap guard): the property root
    /// region (in the combined `[edge root | property root]` span)
    /// would exceed `R_MAX = 1024` entries on the next property-leaf
    /// mint. The fan-out at the property root is the same `K =
    /// R_MAX` as the edge root (the B-radix interior radix is the
    /// same constant), so the cap mirrors `TreeRootCapacityReached`.
    /// Property-tree deepen (with `BlockKind::InlinePropertyInterior`)
    /// would lift the cap; that wire-up is recorded as a follow-up
    /// slice per ADR 0088 §7.
    PropertyTreeRootCapacityReached {
        /// The `next_stored` value the insert would have produced.
        stored_slots: u32,
        /// Property root length at the time of the failed insert.
        property_root_len: u32,
        /// Structural cap (`R_MAX = 1024` per ADR 0088 §4).
        cap: u32,
        /// Property leaf fan-out at the failed insert
        /// (`K = floor(4096 / w)`).
        property_leaf_fanout: u32,
    },
    /// Plan 0320 §F-1: the `materialize_inline_property_stream` reservation
    /// for `(vid, bucket_slot, width, fill_byte, degree)` exceeds
    /// `INLINE_MATERIALIZE_BYTE_BUDGET` (= 4 KiB) and must be filled via the
    /// stepped resumable cursor. The inner graph has no maintenance queue
    /// (the queue is owned by the deferred-maintenance wrapper); the caller-facing
    /// `DeferredBidirectionalLabeledLaraGraph` funnel intercepts this signal, enqueues a
    /// `MaintenanceWorkItem::MaterializeInlinePropertyStreamV2` (tag 7, v2),
    /// drains, and re-runs the original operation. The variant is the
    /// *contract* between inner and outer: it carries the work-item payload
    /// plus the (already-bumped) `degree` so the wrapper can size the
    /// stepped fill exactly.
    InlinePropertyMaterializeDeferredRequired(ValueMigration),
    /// A new bucket's edge span could not be placed; carries the stage and geometry.
    BucketSpanPlacementExhausted(BucketSpanPlacementExhausted),
    /// A tree promotion's source disagreed with itself; names both sides.
    TreePromotionSourceMismatch(TreePromotionSourceMismatch),
    /// A property leaf write would leave the leaf unsupported; names the window.
    PropertyLeafWindowMismatch(PropertyLeafWindowMismatch),
    /// Two blocks of one promotion share an id; the store handed it out twice.
    DuplicateBlockMinted(DuplicateBlockMinted),
    /// The vertex's slice cannot hold the span it asked for; names the layout reason.
    VertexSliceExhausted(VertexSliceExhausted),
    /// A vertex's bucket-descriptor row cannot be located; carries the fields that explain why.
    BucketDescriptorRow(BucketDescriptorRowUnavailable),
}

/// A migration the graph could not finish inline: the work a caller has to drive.
///
/// The entry that decides inline-versus-deferred publishes this value, the error variant carries it, and the
/// bidirectional wrapper queues exactly this; one type, so the three cannot describe the same job differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValueMigration {
    /// Source vertex (already validated).
    pub vid: VertexId,
    /// Bucket slot (already resolved).
    pub bucket_slot: u64,
    /// Target width to publish on the descriptor.
    pub width: u16,
    /// Fill byte (Plan 0320 §0.1 dense-prefix semantics).
    pub fill_byte: u8,
    /// Live-ordinal count at the time of the signal (= `bucket.degree()`).
    pub degree: u32,
}

impl ValueMigration {
    /// Bytes the stepped fill must cover: every live ordinal gets a block, wherever it lives (ADR 0099).
    pub const fn total_bytes(&self) -> u64 {
        self.degree as u64 * self.width as u64
    }
}

/// A vertex whose bucket-descriptor row cannot be located: the row is the vertex's `degree + slack` slots, and that
/// sum is the invariant every descriptor read and write depends on.
///
/// This is its own error rather than an overflow because the two ways it fails need different answers - a
/// default-labeled vertex has no row by design, while a normal vertex whose degree plus slack overflowed is a
/// corruption or a placement bug - and an `overflow()` at the site told a probe neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketDescriptorRowUnavailable {
    /// The vertex whose row could not be located.
    pub vid: VertexId,
    /// The degree the vertex reported.
    pub degree: u32,
    /// The descriptor slack the vertex carried.
    pub slack: u32,
    /// Whether the vertex was default-edge labelled, which has no descriptor row by design.
    pub default_labeled: bool,
}

impl fmt::Display for BucketDescriptorRowUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "vertex {} has no bucket-descriptor row: degree={}, slack={}, default_labeled={}",
            self.vid, self.degree, self.slack, self.default_labeled
        )
    }
}

/// A new bucket's edge span could not be placed: which stage gave up, and the geometry it gave up with.
///
/// The placement path catches `CollectAllocationOverflow` to relocate the leaf and try again, so the value a failing
/// insert reported was the *last tolerated* one - it named neither the stage nor what the layout looked like. This is
/// the error only the final "exhausted" answer produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketSpanPlacementExhausted {
    /// The stage that gave up: `pin`, `relocate`, `base`, `successor`, `read`, or `place`.
    pub stage: &'static str,
    /// The bucket slot the create returned.
    pub slot: u64,
    /// The descriptor index the caller derived.
    pub bucket_index: u32,
    /// The vertex's descriptor-row base.
    pub base: u64,
    /// The vertex's bucket count.
    pub degree: u32,
    /// The descriptor row's slot capacity.
    pub capacity: u64,
    /// How many leaf relocations the placement attempted before giving up.
    pub relocations: u8,
}

impl fmt::Display for BucketSpanPlacementExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bucket span placement exhausted at `{}` for slot {} (index {}, base {}, degree {}, capacity {}), \
             after {} relocation(s)",
            self.stage,
            self.slot,
            self.bucket_index,
            self.base,
            self.degree,
            self.capacity,
            self.relocations
        )
    }
}

/// A vertex's slice cannot hold the span it asked for: the leaf's mates own the space, or the pinned floor does not
/// cover the request.
///
/// This is the ADR 0098 bound answering "no", and it is its own error because it is not a capacity arithmetic
/// overflow: the arithmetic is fine and the *layout* refuses. `GAP-GRAPH-TWO-LABELS-ON-A-BIG-VERTEX` needed exactly
/// this distinction - the fifth edge of a second label on a 10 000-row hub fails here, and every earlier probe read
/// it as a generic overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VertexSliceExhausted {
    /// The vertex whose slice cannot take the span.
    pub vid: VertexId,
    /// How many slots the request needed.
    pub span_len: u64,
    /// The end slot the request needed.
    pub span_end: u64,
    /// The vertex's current content end.
    pub content_end: u64,
    /// Whether the leaf's other mates left the span free.
    pub mates_free: bool,
    /// Whether the leaf is pinned (its floor must cover the span).
    pub pinned: bool,
    /// Whether the pinned floor covers the span, when it is pinned.
    pub floor_covers: bool,
}

impl fmt::Display for VertexSliceExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "vertex {} slice exhausted: span {}..{} refused (content ends at {}, mates_free={}, \
             pinned={}, floor_covers={})",
            self.vid,
            self.span_len,
            self.span_end,
            self.content_end,
            self.mates_free,
            self.pinned,
            self.floor_covers
        )
    }
}

/// A tree promotion's source disagreed with itself: the row the visitor yielded is outside the extent the promotion
/// sized, or the live count does not match the bucket's degree.
///
/// Both used to be a bare `overflow()`, which said "no room" for an answer that is about consistency. Naming the
/// numbers is what makes the disagreement readable without a probe - and the promotion's write side cannot produce it,
/// because its block ids are admitted before transcription.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreePromotionSourceMismatch {
    /// Which check failed: `slot-past-extent` or `live-count`.
    pub stage: &'static str,
    /// The slot the visitor yielded (for `slot-past-extent`), else the last slot seen.
    pub slot: u32,
    /// The extent the promotion sized its blocks for.
    pub extent: u32,
    /// Rows the visitor reported live (for `live-count`).
    pub live: u32,
    /// The bucket's degree.
    pub degree: u32,
}

impl fmt::Display for TreePromotionSourceMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tree promotion source mismatch at `{}`: slot={}, extent={}, live={}, degree={}",
            self.stage, self.slot, self.extent, self.live, self.degree
        )
    }
}

/// A property leaf write that would leave the leaf unsupported: the slot belongs to a window the writer is not
/// filling, so sealing here would publish a leaf no id accounts for - or overwrite one that is already accounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyLeafWindowMismatch {
    /// The slot the caller tried to write.
    pub slot: u32,
    /// First slot of the window being filled.
    pub window_start: u32,
    /// Values one property leaf holds at this width.
    pub slots_per_block: u32,
}

impl fmt::Display for PropertyLeafWindowMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "property leaf window mismatch: slot {} is outside the window starting at {} ({} slots per leaf)",
            self.slot, self.window_start, self.slots_per_block
        )
    }
}

/// Two blocks of one tree promotion were minted with the same id.
///
/// A promotion mints a block per transcribed leaf; if the store hands one id twice, two leaves alias one payload and
/// the descriptor publishes an image whose entries are not distinct - silently, because every id is "valid". This names
/// the aliasing at the point it would be published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DuplicateBlockMinted {
    /// The id that was handed out twice.
    pub id: u32,
}

impl fmt::Display for DuplicateBlockMinted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block id {} was minted twice for one tree promotion: two leaves would alias one payload",
            self.id
        )
    }
}

impl fmt::Display for LabeledOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EdgeSlotOutOfRange { slot, stored_slots } => write!(
                f,
                "edge slot {slot} is outside stored extent {stored_slots}"
            ),
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
            Self::InlinePropertyMaterializationConsumed => {
                write!(f, "materialization cursor already consumed")
            }
            Self::InlinePropertyMaterializationInvalidSource => {
                write!(f, "materialization source is no longer canonical")
            }
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
            Self::TreeModeEdgeWidthUnsupported { edge_bytes } => write!(
                f,
                "tree mode requires a 4-byte edge record, got {edge_bytes} bytes (ADR 0088 §2)"
            ),
            Self::TreeDepthLimitReached { depth, max_depth } => write!(
                f,
                "tree-mode depth limit reached: depth={depth}, max_depth={max_depth}"
            ),
            Self::LeafArenaExhausted { vid, need } => write!(
                f,
                "leaf arena exhausted: {vid:?} cannot place {need} slot(s) inside its leaf \
                 without leaving it"
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
            Self::PropertyTreeRootCapacityReached {
                stored_slots,
                property_root_len,
                cap,
                property_leaf_fanout,
            } => write!(
                f,
                "tree-mode property root at capacity: property_root_len={property_root_len}, \
                 cap={cap}, stored_slots={stored_slots}, property_leaf_fanout={property_leaf_fanout} \
                 (property-tree deepen with BlockKind::InlinePropertyInterior is not yet \
                 wired; follow-up todo: property-tree-deepen)"
            ),
            Self::BucketDescriptorRow(unavailable) => unavailable.fmt(f),
            Self::BucketSpanPlacementExhausted(exhausted) => exhausted.fmt(f),
            Self::TreePromotionSourceMismatch(mismatch) => mismatch.fmt(f),
            Self::PropertyLeafWindowMismatch(mismatch) => mismatch.fmt(f),
            Self::DuplicateBlockMinted(duplicate) => duplicate.fmt(f),
            Self::VertexSliceExhausted(exhausted) => exhausted.fmt(f),
            Self::InlinePropertyMaterializeDeferredRequired(migration) => write!(
                f,
                "inline-property materialization exceeds INLINE_MATERIALIZE_BYTE_BUDGET: \
                 vid={}, bucket_slot={}, width={}, fill_byte=0x{:02x}, degree={}; caller must drain \
                 maintenance queue and retry",
                migration.vid,
                migration.bucket_slot,
                migration.width,
                migration.fill_byte,
                migration.degree
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
            | Self::EdgeSlotOutOfRange { .. }
            | Self::InvalidDefaultBypass
            | Self::InlinePropertyBytesWidthMismatch { .. }
            | Self::InlinePropertyMaterializationConsumed
            | Self::InlinePropertyMaterializationInvalidSource
            | Self::InvalidVertexRow(_)
            | Self::AllocSpaceCapReached { .. }
            | Self::VertexBucketCountCapReached { .. }
            | Self::LtbBlock(_)
            | Self::BucketNotFound { .. }
            | Self::TreeModeEdgeWidthUnsupported { .. }
            | Self::TreeDepthLimitReached { .. }
            | Self::LeafArenaExhausted { .. }
            | Self::TreeRootCapacityReached { .. }
            | Self::PropertyTreeRootCapacityReached { .. }
            | Self::BucketDescriptorRow(_)
            | Self::BucketSpanPlacementExhausted(_)
            | Self::TreePromotionSourceMismatch(_)
            | Self::PropertyLeafWindowMismatch(_)
            | Self::DuplicateBlockMinted(_)
            | Self::VertexSliceExhausted(_)
            | Self::InlinePropertyMaterializeDeferredRequired(_) => None,
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
            | LabeledVertexFieldError::MetadataReservedBitsSet
            | LabeledVertexFieldError::SpillRunOutOfRange
            | LabeledVertexFieldError::SpillRunOnNormalRow
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
            crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesOffsetOverflow
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesWidthOutOfRange
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesLogHeadOutOfRange
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesLogLenOutOfRange
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesLogStateMismatch
            | crate::labeled::record::LabelBucketFieldError::InlinePropertyBytesStateWithoutSchema
            | crate::labeled::record::LabelBucketFieldError::TinyTreeModeConflict
            | crate::labeled::record::LabelBucketFieldError::TinyDegreeOutOfRange
            | crate::labeled::record::LabelBucketFieldError::TinyUsedWidthOutOfRange
            | crate::labeled::record::LabelBucketFieldError::TinyValueStatePresent
            | crate::labeled::record::LabelBucketFieldError::SpillRunOutOfRange
            | crate::labeled::record::LabelBucketFieldError::SpillRunOnNonSlabBucket => {
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

impl From<OutEdgeOrder> for crate::traverse::TraversalOrder {
    fn from(order: OutEdgeOrder) -> Self {
        match order {
            OutEdgeOrder::Ascending => Self::Ascending,
            OutEdgeOrder::Descending => Self::Descending,
        }
    }
}

impl OutEdgeOrder {
    pub(super) fn ascending(self) -> bool {
        matches!(self, Self::Ascending)
    }
}
