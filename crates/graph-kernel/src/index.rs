//! Shared property-index API transport types.

use crate::canonical_export::CanonicalExportError;
use crate::federation::ShardId;
use candid::Encode;
use ic_stable_structures::{Storable, storable::Bound};
use std::borrow::Cow;
use std::fmt;

/// Maximum encoded sortable index value key size (`V` in index capacity planning).
pub const MAX_INDEX_VALUE_KEY_BYTES: usize = 4096;

/// Maximum number of equality arms the bounded server-side intersection (`lookup_intersection_page`)
/// will accept. Shared between Router candidate resolution and Property Index execution so both
/// enforce the same provider-neutral execution bound for pure equality conjunctions.
///
/// `MAX_EQUALITY_INTERSECTION_ARMS` is the execution limit, not a GQL syntax limit. The planner is
/// free to accept longer pure equality conjunctions; the Router lowers 1 arm to a single equality
/// lookup and 2..=MAX_EQUALITY_INTERSECTION_ARMS arms to the bounded intersection. Requests that
/// resolve to more arms are rejected with a provider-specific error before any index canister call.
pub const MAX_EQUALITY_INTERSECTION_ARMS: usize = 8;

/// Maximum number of posting hits materialized by one paginated index read.
///
/// This is an execution/heap bound, not the ICP message-size limit. The receiver still
/// validates the encoded request and response against the shared transport ceiling. Keeping the
/// page bound here makes Graph, Router, and Property Index use the same producer-side budget.
pub const MAX_POSTING_PAGE_HITS: u32 = 10_000;

/// Physical property-index posting namespace.
///
/// One value identifies one build generation and one stable posting namespace. It is intentionally
/// distinct from Router-owned logical index names and property ids, and must never be duplicated by
/// a separate generation field.
#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub struct PhysicalIndexId(#[serde(deserialize_with = "deserialize_physical_index_id_raw")] u64);

fn deserialize_physical_index_id_raw<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <u64 as serde::Deserialize>::deserialize(deserializer)?;
    if raw == 0 {
        return Err(serde::de::Error::custom("physical index id 0 is reserved"));
    }
    Ok(raw)
}

impl PhysicalIndexId {
    #[inline]
    pub const fn new(raw: u64) -> Option<Self> {
        if raw == 0 { None } else { Some(Self(raw)) }
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_reserved(self) -> bool {
        self.0 == 0
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 8] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 8]) -> Option<Self> {
        Self::new(u64::from_le_bytes(bytes))
    }

    /// Allocate the next monotonic namespace without reuse or overflow.
    #[inline]
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Self::new(next),
            None => None,
        }
    }
}

impl Storable for PhysicalIndexId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 8, "PhysicalIndexId expects exactly 8 bytes");
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        Self::from_le_bytes(raw).expect("PhysicalIndexId zero is reserved")
    }
}

/// Lifecycle phase carried with a Router-owned indexed-catalog membership.
///
/// Graph persists this phase with pending and repair work so a future lifecycle-aware
/// dispatcher can route Building/Sealing work to the build protocol. The ordinary
/// posting path is valid only for Active memberships.
#[repr(u8)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub enum IndexMaintenancePhase {
    Building = 1,
    Sealing = 2,
    Active = 3,
}

impl IndexMaintenancePhase {
    #[inline]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// One shard-owned property-index mutation. The index canister applies these in
/// order and may return a continuation before the request is exhausted.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum IndexPostingMutation {
    VertexProperty {
        physical_index_id: PhysicalIndexId,
        remove: bool,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    },
    EdgeProperty {
        physical_index_id: PhysicalIndexId,
        remove: bool,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    },
    Label {
        remove: bool,
        label_id: u32,
        vertex_id: u32,
    },
}

/// A bounded progress result from one property-index update call.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub struct IndexPostingBatchProgress {
    /// Number of operations accepted from the beginning of the request.
    pub applied: u32,
    /// First operation not accepted by this call, if any.
    pub next_index: Option<u32>,
    /// True when the index canister stopped at its own instruction budget.
    pub instruction_budget_exhausted: bool,
}

/// Maximum number of encoded old/new values accepted by one exact build-DML request.
///
/// The canister additionally enforces the shared encoded inter-canister request byte ceiling.
/// Keeping the row bound here prevents a request containing many empty values from bypassing the
/// byte-based limit.
pub const MAX_INDEX_BUILD_DML_VALUES: usize = 1_024;

/// Maximum immutable target-shard count accepted when a graph-index build is registered.
pub const MAX_INDEX_BUILD_TARGET_SHARDS: usize = 4_096;

/// Maximum opaque Graph-owned cursor width persisted by graph-index build progress.
pub const MAX_INDEX_BUILD_CURSOR_BYTES: usize = 16 * 1_024;

/// Maximum Graph pages pulled by one Router-authorized build advance call.
///
/// This is deliberately small: every page is one inter-canister call followed by one atomic
/// graph-index commit. Router resumes the durable worker until [`IndexBuildProgress::done`].
pub const MAX_INDEX_BUILD_ADVANCE_PAGES: u32 = 4;

/// Graph-index-local rejection code carried inside [`IndexBuildError::Store`].
///
/// The wire deliberately excludes implementation-detail strings. Callers classify retryability
/// from the typed code instead of parsing diagnostics.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexBuildStoreError {
    NotAuthorized,
    UnknownShard,
    WrongShardCanister,
    IndexValueKeyTooLarge,
    AlreadyRegistered,
    UnknownBuild,
    StaleEpoch,
    InvalidScope,
    InvalidTargetShards,
    InvalidTarget,
    RequestTooLarge,
    TooManyRows,
    InvalidCursor,
    AlreadyDone,
    StaleProgress,
    ReplayConflict,
    ReplayTooOld,
    InvalidSequence,
    SequenceGap,
    InvalidControl,
    NotBuilding,
    NotReadyToSeal,
    InvalidSeal,
    Aborted,
    DuplicateSubject,
    ProgressOverflow,
    FingerprintFailed,
    Internal,
}

impl IndexBuildStoreError {
    #[inline]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::UnknownShard | Self::StaleProgress | Self::SequenceGap | Self::NotReadyToSeal
        )
    }
}

/// Compact typed failure for graph-index build control and DML endpoints.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexBuildError {
    Store(IndexBuildStoreError),
    Transport,
    Graph(CanonicalExportError),
}

impl IndexBuildError {
    #[inline]
    pub const fn is_retryable(self) -> bool {
        match self {
            Self::Store(error) => error.is_retryable(),
            Self::Transport => true,
            Self::Graph(error) => matches!(
                error,
                CanonicalExportError::RetryableSealing
                    | CanonicalExportError::SequenceGap
                    | CanonicalExportError::NotConverged
                    | CanonicalExportError::UnsafeRemoval
                    | CanonicalExportError::Storage
            ),
        }
    }
}

/// Immutable logical property scope for one physical posting namespace.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexBuildTarget {
    Vertex {
        label_id: u16,
        property_id: crate::entry::PropertyId,
    },
    Edge {
        label_id: u16,
        property_id: crate::entry::PropertyId,
        direction: EdgeIndexDirection,
    },
}

impl IndexBuildTarget {
    #[inline]
    pub const fn property_id(self) -> crate::entry::PropertyId {
        match self {
            Self::Vertex { property_id, .. } | Self::Edge { property_id, .. } => property_id,
        }
    }
}

/// Router-authorized immutable registration envelope for one graph-index-owned build.
///
/// Recurring DML and seed-page requests carry only the physical id, epoch, and current work. The
/// complete scope and fixed shard set are sent once here and remain the receiver's source of truth.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct RegisterIndexBuildRequest {
    pub physical_index_id: PhysicalIndexId,
    pub graph_id: crate::entry::GraphId,
    pub index_name_id: crate::entry::IndexNameId,
    pub catalog_epoch: u64,
    /// Immutable Router topology snapshot captured before the physical build was registered.
    pub topology_epoch: u64,
    pub target: IndexBuildTarget,
    /// Raw `ShardId`s. Candid represents the domain newtype as `nat32`, but nested newtype serde
    /// decoding is not self-roundtrippable, so bounded shard vectors use their canonical raw form.
    pub target_shard_ids: Vec<u32>,
}

/// Fixed-width canonical identity whose posting may be superseded by concurrent DML during an
/// online physical-index build.
///
/// A vertex id is shard-local. An edge occurrence is the Graph-owned canonical
/// `(owner_vertex_id, label_id, slot_index)` occurrence within one shard. Neither identity carries
/// a property value, logical name, or a second build-generation field.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub enum IndexBuildSubject {
    Vertex {
        shard_id: u32,
        vertex_id: u32,
    },
    Edge {
        shard_id: u32,
        owner_vertex_id: u32,
        label_id: u16,
        slot_index: u32,
    },
}

impl IndexBuildSubject {
    #[inline]
    pub const fn shard_id(self) -> ShardId {
        match self {
            Self::Vertex { shard_id, .. } | Self::Edge { shard_id, .. } => ShardId::new(shard_id),
        }
    }
}

/// Exact namespace-scoped posting correction emitted by canonical Graph DML during a build.
///
/// The receiver validates the persisted build scope and the complete removal/insertion envelope
/// before marking `subject` touched and applying the set mutations. Replaying the same envelope is
/// idempotent because posting and touched storage are stable sets.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildDmlRequest {
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    /// One-based contiguous sequence within `subject.shard_id()` for this physical namespace.
    pub shard_sequence: u64,
    pub subject: IndexBuildSubject,
    pub removals: Vec<Vec<u8>>,
    pub insertions: Vec<Vec<u8>>,
}

impl IndexBuildDmlRequest {
    /// Stable domain-separated fingerprint used for the one-entry exact replay receipt per shard.
    pub fn fingerprint(&self) -> Result<[u8; 32], candid::Error> {
        use sha2::{Digest, Sha256};

        let encoded = candid::Encode!(self)?;
        let mut hasher = Sha256::new();
        hasher.update(b"gleaph:index-build-dml:v1\0");
        hasher.update((encoded.len() as u64).to_le_bytes());
        hasher.update(encoded);
        Ok(hasher.finalize().into())
    }
}

/// Exact Router control identity for one registered physical build.
///
/// Carrying the immutable registration makes stale or cross-target control calls fail before a
/// Graph call, seal transition, or cleanup mutation.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildControlRequest {
    pub registration: RegisterIndexBuildRequest,
}

/// Captured Graph outbox admission watermark for one target shard at the seal boundary.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexBuildSealTarget {
    pub shard_id: u32,
    pub admitted_through: u64,
}

/// Router-only transition from Building to Sealing.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildSealRequest {
    pub control: IndexBuildControlRequest,
    /// Fresh Router catalog epoch fencing new affected-index DML admission.
    pub seal_catalog_epoch: u64,
    /// Exact sorted target-shard set with the last old-epoch sequence admitted by each Graph.
    pub shard_targets: Vec<IndexBuildSealTarget>,
}

/// Durable old-epoch drain proof for one target shard.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexBuildShardWatermark {
    pub shard_id: u32,
    pub admitted_through: u64,
    pub drained_through: u64,
}

/// Graph-index-owned lifecycle. `Active` is intentionally absent because publication is Router
/// catalog state, not graph-index maintenance state.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexBuildPhase {
    Building,
    Sealing { seal_catalog_epoch: u64 },
    Aborting,
    Aborted,
}

/// One canonical Graph export page ready for graph-index-owned base seeding.
///
/// `expected_cursor` is the cursor persisted before this page was fetched. `next_cursor` and
/// `done` are the Graph-owned page result. The explicit sequence plus an envelope fingerprint lets
/// graph-index return an exact replay receipt after it has already advanced the durable cursor.
#[derive(Clone, Debug, PartialEq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildSeedPageRequest {
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub page_sequence: u64,
    pub shard_id: u32,
    pub expected_cursor: Option<Vec<u8>>,
    pub facts: Vec<crate::canonical_export::CanonicalIndexableFact>,
    pub next_cursor: Option<Vec<u8>>,
    pub done: bool,
}

impl IndexBuildSeedPageRequest {
    /// Stable domain-separated fingerprint of the exact page envelope.
    pub fn fingerprint(&self) -> Result<[u8; 32], candid::Error> {
        use sha2::{Digest, Sha256};

        let encoded = candid::Encode!(self)?;
        let mut hasher = Sha256::new();
        hasher.update(b"gleaph:index-build-seed-page:v1\0");
        hasher.update((encoded.len() as u64).to_le_bytes());
        hasher.update(encoded);
        Ok(hasher.finalize().into())
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexBuildSeedDisposition {
    Applied,
    Replay,
}

/// Public progress after one accepted or exact-replayed seed page.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildProgress {
    pub next_page_sequence: u64,
    pub next_shard_index: u32,
    pub expected_shard_id: Option<u32>,
    pub cursor: Option<Vec<u8>>,
    pub seeded_items: u64,
    pub done: bool,
}

/// Public durable status for one registered physical build.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildStatus {
    pub registration: RegisterIndexBuildRequest,
    pub progress: IndexBuildProgress,
    pub phase: IndexBuildPhase,
    /// Sorted exact target-shard watermarks. Before sealing, `admitted_through` equals the current
    /// acknowledged watermark. During sealing it remains the captured target.
    pub watermarks: Vec<IndexBuildShardWatermark>,
}

/// Seal registration/drain status returned to Router.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildSealStatus {
    pub base_complete: bool,
    pub seal_catalog_epoch: u64,
    pub watermarks: Vec<IndexBuildShardWatermark>,
}

/// One bounded, resumable namespace-cleanup result.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexBuildCleanupStatus {
    pub done: bool,
}

/// Stable-result shape returned by base seeding. Exact replay changes only `disposition`; the
/// persisted progress and page counts are the original successful result.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexBuildSeedPageResult {
    pub disposition: IndexBuildSeedDisposition,
    pub inserted_facts: u32,
    pub skipped_touched_facts: u32,
    pub progress: IndexBuildProgress,
}

/// Returned when encoded index value key bytes exceed [`MAX_INDEX_VALUE_KEY_BYTES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexValueKeyTooLarge {
    pub len: usize,
}

impl fmt::Display for IndexValueKeyTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "index value key length {} exceeds maximum {} bytes",
            self.len, MAX_INDEX_VALUE_KEY_BYTES
        )
    }
}

impl std::error::Error for IndexValueKeyTooLarge {}

/// Validates that encoded index value key bytes are within [`MAX_INDEX_VALUE_KEY_BYTES`].
pub fn validate_index_value_key_bytes(value: &[u8]) -> Result<(), IndexValueKeyTooLarge> {
    if value.len() <= MAX_INDEX_VALUE_KEY_BYTES {
        Ok(())
    } else {
        Err(IndexValueKeyTooLarge { len: value.len() })
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct PostingHit {
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Vertex or edge property equality arm for [`IndexIntersectionRequest`] (ADR 0009 §3).
#[derive(
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
pub enum IndexSubject {
    #[default]
    VertexProperty,
    EdgeProperty {
        label_id: Option<u16>,
    },
}

/// One equality arm for [`IndexIntersectionRequest`].
///
/// `value` must be the sortable index key from `gleaph_gql::value_to_index_key_bytes`.
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexEqualSpec {
    pub physical_index_id: PhysicalIndexId,
    pub subject: IndexSubject,
    pub property_id: u32,
    pub value: Vec<u8>,
}

impl IndexEqualSpec {
    pub fn vertex(physical_index_id: PhysicalIndexId, property_id: u32, value: Vec<u8>) -> Self {
        Self {
            physical_index_id,
            subject: IndexSubject::VertexProperty,
            property_id,
            value,
        }
    }

    pub fn edge(
        physical_index_id: PhysicalIndexId,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Self {
        Self {
            physical_index_id,
            subject: IndexSubject::EdgeProperty { label_id },
            property_id,
            value,
        }
    }
}

/// Result of a multi-property index intersection (ADR 0009 §3).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum IndexIntersectionResult {
    Vertices(Vec<PostingHit>),
    Edges(Vec<EdgePostingHit>),
}

impl IndexIntersectionResult {
    pub fn vertices(self) -> Vec<PostingHit> {
        match self {
            Self::Vertices(hits) => hits,
            Self::Edges(_) => Vec::new(),
        }
    }
}

/// Intersect equality postings for multiple properties (at least two specs; Eq only).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexIntersectionRequest {
    pub specs: Vec<IndexEqualSpec>,
}

/// Intersect label membership postings for multiple labels (at least two label ids).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexLabelIntersectionRequest {
    pub vertex_label_ids: Vec<u32>,
}

/// Resume cursor for [`LabelLookupPageRequest`] (last posting from the prior page).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct LabelPostingCursor {
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Paginated label membership export scoped to one shard.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LabelLookupPageRequest {
    pub vertex_label_id: u32,
    pub shard_id: ShardId,
    pub after: Option<LabelPostingCursor>,
    pub limit: u32,
}

/// Paginated label intersection export scoped to one shard, with all label sieves applied inside
/// the index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LabelIntersectionPageRequest {
    pub walk_label_id: u32,
    pub sieve_label_ids: Vec<u32>,
    pub shard_id: ShardId,
    pub after: Option<LabelPostingCursor>,
    pub limit: u32,
}

/// One page of label postings for a shard-local export.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LabelLookupPageResult {
    pub hits: Vec<PostingHit>,
    /// Last hit in this page; pass as `after` on the next request when `done` is false.
    pub next: Option<LabelPostingCursor>,
    pub done: bool,
}

/// Compare encoded property values using the same lexicographic order as index posting keys.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum PostingRangeRequest {
    Ge(Vec<u8>),
    Gt(Vec<u8>),
    Le(Vec<u8>),
    Lt(Vec<u8>),
    /// Finite half-open encoded interval `[low, high)` supplied by the caller.
    /// Property Index validates the bounds structurally and scans only that interval.
    Between {
        low: Vec<u8>,
        high: Vec<u8>,
    },
}

/// Global posting cardinality for one encoded property value (all shards).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCount {
    pub encoded_value: Vec<u8>,
    pub count: u64,
}

/// Resume cursor for grouped count scans. The next page starts strictly after this value.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCountCursor {
    pub encoded_value: Vec<u8>,
}

/// Maximum groups returned by one grouped-count page. At the maximum encoded value width this
/// keeps the value payload below the conservative 2 MiB inter-canister budget with headroom.
pub const MAX_VALUE_POSTING_COUNT_PAGE_GROUPS: u32 = 256;

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupValuePostingCountPageRequest {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub min_count: u64,
    pub vertex_filter_packed: Option<Vec<u64>>,
    pub after: Option<ValuePostingCountCursor>,
    pub limit: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct ValuePostingCountPage {
    pub counts: Vec<ValuePostingCount>,
    pub next: Option<ValuePostingCountCursor>,
    pub done: bool,
}

/// Resume cursor for paginated vertex property exports (last posting from the prior page).
///
/// Carries `value` because range scans span multiple encoded values; equality scans repeat the
/// fixed request value.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PropertyPostingCursor {
    pub value: Vec<u8>,
    pub shard_id: ShardId,
    pub vertex_id: u32,
}

/// Resume cursor for paginated edge property exports (last posting from the prior page).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct EdgePostingCursor {
    pub value: Vec<u8>,
    pub label_id: u16,
    pub shard_id: ShardId,
    pub owner_vertex_id: u32,
    pub slot_index: u32,
}

/// One page of vertex property postings (equality or range export).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PostingHitPage {
    pub hits: Vec<PostingHit>,
    /// Last hit in this page; pass as `after` on the next request when `done` is false.
    pub next: Option<PropertyPostingCursor>,
    pub done: bool,
}

/// Cursor used by the paginated intersection export. The cursor variant identifies the
/// canonical walk arm selected by the Property Index for the requested intersection shape.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum IntersectionPostingCursor {
    Vertex(PropertyPostingCursor),
    Edge(EdgePostingCursor),
}

/// Paginated property intersection export for vertex, edge, and mixed equality shapes.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupPropertyIntersectionPageRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub after: Option<IntersectionPostingCursor>,
    pub limit: u32,
}

/// One bounded page of a property intersection result.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PropertyIntersectionPage {
    pub hits: IndexIntersectionResult,
    pub next: Option<IntersectionPostingCursor>,
    pub done: bool,
}

/// One page of edge property postings (equality export).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct EdgePostingHitPage {
    pub hits: Vec<EdgePostingHit>,
    /// Last hit in this page; pass as `after` on the next request when `done` is false.
    pub next: Option<EdgePostingCursor>,
    pub done: bool,
}

/// Paginated equality export for one `(property_id, value)` vertex bucket.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualPageRequest {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub value: Vec<u8>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated equality export with label membership applied inside the index canister.
///
/// Keeping the label sieve at the producer avoids sending a page to the Router and then
/// performing a second inter-canister query for the same page.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualPageForLabelRequest {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub value: Vec<u8>,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated all-vertex equality intersection (the planner's `IndexIntersection` shape).
///
/// The index walks the first arm (`specs[0]`) in pages and sieves each page against the remaining
/// arms server-side, returning at most `limit` surviving hits plus a [`PropertyPostingCursor`] over
/// the walk arm. This keeps the per-message heap bounded (no per-arm set is materialized) and folds
/// the walk + sieve into a single inter-canister call per page (vs one call per arm per page).
///
/// Execution contract:
/// - `specs` must contain between 2 and [`MAX_EQUALITY_INTERSECTION_ARMS`] arms inclusive; fewer
///   arms produce an empty terminal page.
/// - All specs must target [`IndexSubject::VertexProperty`]; edge or mixed specs produce an empty
///   terminal page (the materializing [`IndexIntersectionRequest`] handles those shapes).
/// - The walk arm is selected deterministically by canonical `(property_id, value)` order, so
///   repeated requests and different callers converge to the same walk plan and resume cursor.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupIntersectionPageRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated all-vertex equality intersection with vertex-label membership applied inside the
/// index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupIntersectionPageForLabelRequest {
    pub specs: Vec<IndexEqualSpec>,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range export over encoded values for one vertex property.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangePageRequest {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub range: PostingRangeRequest,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range export with label membership applied inside the index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangePageForLabelRequest {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub range: PostingRangeRequest,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range-plus-equality intersection over one vertex property range walk.
///
/// The index walks the finite encoded half-open interval `[low, high)` for `range_property_id`
/// one page at a time and sieves each page against every equality spec in `equal_specs`
/// server-side. The returned `next` and `done` always describe the range walk, even when a page
/// has zero survivors. This keeps the per-message heap bounded: no full range or equality bucket
/// is materialized.
///
/// Execution contract:
/// - `equal_specs` must contain between 1 and [`MAX_EQUALITY_INTERSECTION_ARMS`] vertex specs
///   inclusive. Zero specs is an invalid request shape; callers with no equality sieve should use
///   [`LookupRangePageRequest`] instead. More than [`MAX_EQUALITY_INTERSECTION_ARMS`] specs are
///   rejected by the Property Index.
/// - All specs must target [`IndexSubject::VertexProperty`]. Edge or mixed specs are rejected.
/// - The Property Index canonicalises sieve order by `(property_id, encoded_value)` for
///   deterministic paging; repeated requests and different callers converge to the same walk plan
///   and resume cursor.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangeIntersectionPageRequest {
    pub range_physical_index_id: PhysicalIndexId,
    pub range_property_id: u32,
    pub low: Vec<u8>,
    pub high: Vec<u8>,
    pub equal_specs: Vec<IndexEqualSpec>,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated range-plus-equality intersection with vertex-label membership applied inside the
/// index canister.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupRangeIntersectionPageForLabelRequest {
    pub range_physical_index_id: PhysicalIndexId,
    pub range_property_id: u32,
    pub low: Vec<u8>,
    pub high: Vec<u8>,
    pub equal_specs: Vec<IndexEqualSpec>,
    pub vertex_label_id: u32,
    pub after: Option<PropertyPostingCursor>,
    pub limit: u32,
}

/// Paginated equality export for one edge property `(property_id, value[, label_id])` bucket.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualPageRequest {
    pub physical_index_id: PhysicalIndexId,
    pub property_id: u32,
    pub value: Vec<u8>,
    pub label_id: Option<u16>,
    pub after: Option<EdgePostingCursor>,
    pub limit: u32,
}
/// Batch paginated equality export for many vertex `(property_id, value)` buckets in one call.
///
/// Buckets are answered in `specs` order at **bucket granularity**: a bucket is either fully
/// included in the response or left for the resume cursor — the response payload budget is never
/// allowed to cut a bucket in half, so a caller can treat each returned page as a complete bucket
/// page. Each bucket's page keeps the `done`/`next` semantics of [`LookupEqualPageRequest`], so a
/// bucket with more hits than `limit` is continued through that existing per-bucket endpoint.
/// All specs must target [`IndexSubject::VertexProperty`]; edge buckets belong to
/// [`LookupEdgeEqualBatchRequest`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualBatchRequest {
    pub specs: Vec<IndexEqualSpec>,
    /// Maximum hits per bucket per call (same producer-side bound as [`LookupEqualPageRequest`]).
    pub limit: u32,
}

/// Per-bucket page result for [`LookupEqualBatchRequest`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEqualBatchResult {
    /// Pages in the same order as the answered prefix of `specs`.
    pub pages: Vec<PostingHitPage>,
    /// Index into `specs` of the first bucket not answered, because the response reached its
    /// payload budget or the canister neared its instruction budget. `None` when every spec was
    /// answered. Resume by re-sending `specs[next..]`.
    pub next: Option<u32>,
}

/// Batch paginated equality export for many edge `(property_id, value[, label_id])` buckets.
/// Same bucket-granularity and resume contract as [`LookupEqualBatchRequest`]; every spec must
/// target [`IndexSubject::EdgeProperty`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualBatchRequest {
    pub specs: Vec<IndexEqualSpec>,
    /// Maximum hits per bucket per call (same producer-side bound as [`LookupEdgeEqualPageRequest`]).
    pub limit: u32,
}

/// Per-bucket page result for [`LookupEdgeEqualBatchRequest`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct LookupEdgeEqualBatchResult {
    pub pages: Vec<EdgePostingHitPage>,
    pub next: Option<u32>,
}

/// Host entity for an administrator-registered property index (ADR 0009).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub enum IndexedPropertyKind {
    Vertex,
    Edge,
}

/// Logical edge-index direction represented by an indexed catalog membership.
///
/// The variants mirror the GQL direction lattice. Graph uses the semantic inclusion helpers
/// rather than interpreting raw bytes, so the Router remains the source of direction semantics.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    candid::CandidType,
    serde::Deserialize,
    serde::Serialize,
)]
#[repr(u8)]
pub enum EdgeIndexDirection {
    Outgoing = 1,
    Incoming = 2,
    OutgoingOrIncoming = 3,
    Undirected = 4,
    OutgoingOrUndirected = 5,
    IncomingOrUndirected = 6,
    Any = 7,
}

impl EdgeIndexDirection {
    #[inline]
    pub const fn includes_directed(self) -> bool {
        matches!(
            self,
            Self::Outgoing
                | Self::Incoming
                | Self::OutgoingOrIncoming
                | Self::OutgoingOrUndirected
                | Self::IncomingOrUndirected
                | Self::Any
        )
    }

    #[inline]
    pub const fn includes_undirected(self) -> bool {
        matches!(
            self,
            Self::Undirected | Self::OutgoingOrUndirected | Self::IncomingOrUndirected | Self::Any
        )
    }
}

/// One vertex index membership in the Router-sourced operation catalog.
///
/// The physical namespace is allocated and owned by Router. Graph may cache this projection for
/// the operation, but it must never derive or substitute a namespace locally.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexedVertexMembership {
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub phase: IndexMaintenancePhase,
    pub property_id: u32,
    pub label_id: u16,
}

/// Router → graph shard: register one property for index maintenance.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct RegisterIndexedPropertyArgs {
    pub kind: IndexedPropertyKind,
    pub property_id: u32,
}

/// Router → graph shard: register one edge index `(label, property, direction)` (ADR 0012).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct RegisterIndexedEdgeIndexArgs {
    pub label_id: u16,
    pub property_id: u32,
    pub direction: EdgeIndexDirection,
}

/// One edge index membership `(label, property, direction)` in [`IndexedPropertyCatalog`].
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct IndexedEdgeMembership {
    pub physical_index_id: PhysicalIndexId,
    pub catalog_epoch: u64,
    pub phase: IndexMaintenancePhase,
    pub label_id: u16,
    pub property_id: u32,
    pub direction: EdgeIndexDirection,
    /// Empty for a scalar/top-level edge property; the canonical dotted path for an inline
    /// struct leaf otherwise (for example, `stats.score`).
    pub field_path: String,
}

/// Router-sourced snapshot of which properties are indexed (ADR 0023 D1/D3).
///
/// The router (definitions SSOT) supplies this per operation so the graph shard
/// never persists derived index state. It is consulted ephemerally (set at the
/// start of an operation, cleared at the end) and is therefore immune to the
/// upgrade boundary that made the former shard-local registry stale.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct IndexedPropertyCatalog {
    pub vertex_indexes: Vec<IndexedVertexMembership>,
    pub edge_indexes: Vec<IndexedEdgeMembership>,
}

impl IndexedPropertyCatalog {
    pub fn is_empty(&self) -> bool {
        self.vertex_indexes.is_empty() && self.edge_indexes.is_empty()
    }
}

/// One edge equality posting hit from graph-index (ADR 0009 §1).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize,
)]
pub struct EdgePostingHit {
    pub shard_id: ShardId,
    pub owner_vertex_id: u32,
    pub label_id: u16,
    pub slot_index: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    const PHYSICAL_INDEX_ID: PhysicalIndexId = PhysicalIndexId::new(1).expect("test physical id");

    #[derive(candid::CandidType, serde::Serialize)]
    struct UncheckedPhysicalIndexId(u64);

    #[derive(Debug, PartialEq, candid::CandidType, serde::Deserialize, serde::Serialize)]
    struct TypedShardVector {
        target_shard_ids: Vec<ShardId>,
    }

    #[test]
    fn physical_index_id_reserves_zero_and_advances_monotonically() {
        assert_eq!(PhysicalIndexId::new(0), None);
        assert_eq!(PHYSICAL_INDEX_ID.raw(), 1);
        assert_eq!(
            PHYSICAL_INDEX_ID.checked_next().map(PhysicalIndexId::raw),
            Some(2)
        );
        assert_eq!(
            PhysicalIndexId::new(u64::MAX)
                .expect("non-zero max id")
                .checked_next(),
            None
        );
    }

    #[test]
    fn physical_index_id_candid_rejects_reserved_zero() {
        let valid = Encode!(&PHYSICAL_INDEX_ID).expect("encode physical index id");
        assert_eq!(
            Decode!(&valid, PhysicalIndexId).expect("decode valid physical index id"),
            PHYSICAL_INDEX_ID
        );

        let reserved = Encode!(&UncheckedPhysicalIndexId(0)).expect("encode unchecked id");
        assert!(Decode!(&reserved, PhysicalIndexId).is_err());
    }

    #[test]
    fn typed_shard_vector_candid_decode_is_not_self_roundtrippable() {
        let request = TypedShardVector {
            target_shard_ids: vec![ShardId::new(0), ShardId::new(7)],
        };
        let encoded = Encode!(&request).expect("encode typed shard vector");
        assert!(Decode!(&encoded, TypedShardVector).is_err());
    }

    #[test]
    fn index_build_public_wire_candid_roundtrip() {
        let registration = RegisterIndexBuildRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            graph_id: crate::entry::GraphId::from_raw(3),
            index_name_id: crate::entry::IndexNameId::from_raw(5),
            catalog_epoch: 7,
            topology_epoch: 17,
            target: IndexBuildTarget::Edge {
                label_id: 11,
                property_id: crate::entry::PropertyId::from_raw(13),
                direction: EdgeIndexDirection::Outgoing,
            },
            target_shard_ids: vec![0, 7],
        };
        let encoded = Encode!(&registration).expect("encode registration");
        assert_eq!(
            Decode!(&encoded, RegisterIndexBuildRequest).expect("decode registration"),
            registration
        );

        let control = IndexBuildControlRequest {
            registration: registration.clone(),
        };
        let seal = IndexBuildSealRequest {
            control: control.clone(),
            seal_catalog_epoch: 8,
            shard_targets: vec![
                IndexBuildSealTarget {
                    shard_id: 0,
                    admitted_through: 3,
                },
                IndexBuildSealTarget {
                    shard_id: 7,
                    admitted_through: 5,
                },
            ],
        };
        let encoded = Encode!(&seal).expect("encode seal");
        assert_eq!(
            Decode!(&encoded, IndexBuildSealRequest).expect("decode seal"),
            seal
        );

        let status = IndexBuildStatus {
            registration: registration.clone(),
            progress: IndexBuildProgress {
                next_page_sequence: 2,
                next_shard_index: 1,
                expected_shard_id: Some(7),
                cursor: Some(vec![9]),
                seeded_items: 11,
                done: false,
            },
            phase: IndexBuildPhase::Sealing {
                seal_catalog_epoch: 8,
            },
            watermarks: vec![IndexBuildShardWatermark {
                shard_id: 0,
                admitted_through: 3,
                drained_through: 3,
            }],
        };
        let encoded = Encode!(&status).expect("encode status");
        assert_eq!(
            Decode!(&encoded, IndexBuildStatus).expect("decode status"),
            status
        );

        let dml = IndexBuildDmlRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            catalog_epoch: 7,
            shard_sequence: 1,
            subject: IndexBuildSubject::Edge {
                shard_id: 7,
                owner_vertex_id: 17,
                label_id: 11,
                slot_index: 19,
            },
            removals: vec![vec![1]],
            insertions: vec![vec![2]],
        };
        let encoded = Encode!(&dml).expect("encode DML");
        assert_eq!(
            Decode!(&encoded, IndexBuildDmlRequest).expect("decode DML"),
            dml
        );

        let seed = IndexBuildSeedPageRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            catalog_epoch: 7,
            page_sequence: 23,
            shard_id: 7,
            expected_cursor: Some(vec![3]),
            facts: vec![crate::canonical_export::CanonicalIndexableFact::Edge {
                owner_vertex_id: 17,
                label_id: 11,
                slot_index: 19,
                property_id: crate::entry::PropertyId::from_raw(13),
                encoded_value: vec![4],
            }],
            next_cursor: Some(vec![5]),
            done: false,
        };
        let encoded = Encode!(&seed).expect("encode seed page");
        assert_eq!(
            Decode!(&encoded, IndexBuildSeedPageRequest).expect("decode seed page"),
            seed
        );
    }

    #[test]
    fn mixed_intersection_request_candid_roundtrip() {
        let req = IndexIntersectionRequest {
            specs: vec![
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 1, vec![1, 2]),
                IndexEqualSpec::edge(PHYSICAL_INDEX_ID, 2, vec![3], Some(7)),
            ],
        };
        let bytes = Encode!(&req).expect("encode");
        let decoded: IndexIntersectionRequest = Decode!(&bytes, IndexIntersectionRequest).unwrap();
        assert_eq!(decoded, req);
    }

    #[test]
    fn validate_index_value_key_bytes_at_and_over_limit() {
        let at_limit = vec![0u8; MAX_INDEX_VALUE_KEY_BYTES];
        validate_index_value_key_bytes(&at_limit).expect("at limit");
        let over = vec![0u8; MAX_INDEX_VALUE_KEY_BYTES + 1];
        let err = validate_index_value_key_bytes(&over).expect_err("over limit");
        assert_eq!(err.len, MAX_INDEX_VALUE_KEY_BYTES + 1);
    }

    #[test]
    fn batched_lookup_requests_candid_roundtrip() {
        let req = LookupEqualBatchRequest {
            specs: vec![
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 1, vec![1, 2]),
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 2, vec![3]),
            ],
            limit: 128,
        };
        let bytes = Encode!(&req).expect("encode equal batch");
        assert_eq!(
            Decode!(&bytes, LookupEqualBatchRequest).expect("decode equal batch"),
            req
        );

        let result = LookupEqualBatchResult {
            pages: vec![PostingHitPage {
                hits: vec![PostingHit {
                    shard_id: ShardId::new(0),
                    vertex_id: 7,
                }],
                next: Some(PropertyPostingCursor {
                    value: vec![9],
                    shard_id: ShardId::new(0),
                    vertex_id: 8,
                }),
                done: false,
            }],
            next: Some(1),
        };
        let bytes = Encode!(&result).expect("encode equal batch result");
        assert_eq!(
            Decode!(&bytes, LookupEqualBatchResult).expect("decode equal batch result"),
            result
        );

        let edge_req = LookupEdgeEqualBatchRequest {
            specs: vec![IndexEqualSpec::edge(PHYSICAL_INDEX_ID, 3, vec![5], Some(7))],
            limit: 64,
        };
        let bytes = Encode!(&edge_req).expect("encode edge batch");
        assert_eq!(
            Decode!(&bytes, LookupEdgeEqualBatchRequest).expect("decode edge batch"),
            edge_req
        );
    }

    #[test]
    fn paginated_property_requests_candid_roundtrip() {
        let cursor = PropertyPostingCursor {
            value: vec![1, 2, 3],
            shard_id: ShardId::new(2),
            vertex_id: 7,
        };
        let equal = LookupEqualPageRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 4,
            value: vec![9, 9],
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&equal).expect("encode equal");
        assert_eq!(
            Decode!(&bytes, LookupEqualPageRequest).expect("decode equal"),
            equal
        );

        let range = LookupRangePageRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 4,
            range: PostingRangeRequest::Ge(vec![5]),
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range).expect("encode range");
        assert_eq!(
            Decode!(&bytes, LookupRangePageRequest).expect("decode range"),
            range
        );

        let bounded_range = LookupRangePageRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 4,
            range: PostingRangeRequest::Between {
                low: vec![2, 0, 128, 0, 0, 0],
                high: vec![2, 2, 128, 0, 0, 0],
            },
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&bounded_range).expect("encode bounded range");
        assert_eq!(
            Decode!(&bytes, LookupRangePageRequest).expect("decode bounded range"),
            bounded_range
        );

        let range_intersection = LookupRangeIntersectionPageRequest {
            range_physical_index_id: PHYSICAL_INDEX_ID,
            range_property_id: 5,
            low: vec![2, 0, 128, 0, 0, 0],
            high: vec![2, 2, 128, 0, 0, 0],
            equal_specs: vec![
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 3, vec![7, 8]),
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 9, vec![1, 2]),
            ],
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range_intersection).expect("encode range intersection");
        assert_eq!(
            Decode!(&bytes, LookupRangeIntersectionPageRequest).expect("decode range intersection"),
            range_intersection
        );

        let range_intersection_one = LookupRangeIntersectionPageRequest {
            range_physical_index_id: PHYSICAL_INDEX_ID,
            range_property_id: 6,
            low: vec![2, 0, 128, 0, 0, 0],
            high: vec![2, 2, 128, 0, 0, 0],
            equal_specs: vec![IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 3, vec![7, 8])],
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range_intersection_one).expect("encode range intersection one");
        assert_eq!(
            Decode!(&bytes, LookupRangeIntersectionPageRequest)
                .expect("decode range intersection one"),
            range_intersection_one
        );

        let range_intersection_max = LookupRangeIntersectionPageRequest {
            range_physical_index_id: PHYSICAL_INDEX_ID,
            range_property_id: 6,
            low: vec![2, 0, 128, 0, 0, 0],
            high: vec![2, 2, 128, 0, 0, 0],
            equal_specs: (1..=MAX_EQUALITY_INTERSECTION_ARMS)
                .map(|i| IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, i as u32, vec![i as u8]))
                .collect(),
            after: Some(cursor.clone()),
            limit: 256,
        };
        let bytes = Encode!(&range_intersection_max).expect("encode range intersection max");
        assert_eq!(
            Decode!(&bytes, LookupRangeIntersectionPageRequest)
                .expect("decode range intersection max"),
            range_intersection_max
        );

        let page = PostingHitPage {
            hits: vec![PostingHit {
                shard_id: ShardId::new(2),
                vertex_id: 7,
            }],
            next: Some(cursor.clone()),
            done: false,
        };
        let bytes = Encode!(&page).expect("encode page");
        assert_eq!(Decode!(&bytes, PostingHitPage).expect("decode page"), page);

        let intersection = LookupIntersectionPageRequest {
            specs: vec![
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 1, vec![1]),
                IndexEqualSpec::vertex(PHYSICAL_INDEX_ID, 2, vec![2]),
            ],
            after: Some(cursor),
            limit: 256,
        };
        let bytes = Encode!(&intersection).expect("encode intersection page");
        assert_eq!(
            Decode!(&bytes, LookupIntersectionPageRequest).expect("decode intersection page"),
            intersection
        );
    }

    #[test]
    fn paginated_edge_request_candid_roundtrip() {
        let cursor = EdgePostingCursor {
            value: vec![1],
            label_id: 5,
            shard_id: ShardId::new(0),
            owner_vertex_id: 4,
            slot_index: 1,
        };
        let req = LookupEdgeEqualPageRequest {
            physical_index_id: PHYSICAL_INDEX_ID,
            property_id: 2,
            value: vec![3, 4],
            label_id: Some(5),
            after: Some(cursor.clone()),
            limit: 128,
        };
        let bytes = Encode!(&req).expect("encode edge req");
        assert_eq!(
            Decode!(&bytes, LookupEdgeEqualPageRequest).expect("decode edge req"),
            req
        );

        let page = EdgePostingHitPage {
            hits: vec![EdgePostingHit {
                shard_id: ShardId::new(0),
                owner_vertex_id: 4,
                label_id: 5,
                slot_index: 1,
            }],
            next: Some(cursor),
            done: true,
        };
        let bytes = Encode!(&page).expect("encode edge page");
        assert_eq!(
            Decode!(&bytes, EdgePostingHitPage).expect("decode edge page"),
            page
        );
    }

    #[test]
    fn intersection_result_variants_candid_roundtrip() {
        let vertices = IndexIntersectionResult::Vertices(vec![PostingHit {
            shard_id: ShardId::new(1),
            vertex_id: 9,
        }]);
        let bytes = Encode!(&vertices).expect("encode vertices");
        let decoded: IndexIntersectionResult =
            Decode!(&bytes, IndexIntersectionResult).expect("decode vertices");
        assert_eq!(decoded, vertices);

        let edges = IndexIntersectionResult::Edges(vec![EdgePostingHit {
            shard_id: ShardId::new(0),
            owner_vertex_id: 4,
            label_id: 2,
            slot_index: 1,
        }]);
        let bytes = Encode!(&edges).expect("encode edges");
        let decoded: IndexIntersectionResult =
            Decode!(&bytes, IndexIntersectionResult).expect("decode edges");
        assert_eq!(decoded, edges);
    }

    #[test]
    fn posting_batch_wire_roundtrip_preserves_order_and_progress() {
        let operations = vec![
            IndexPostingMutation::VertexProperty {
                physical_index_id: PHYSICAL_INDEX_ID,
                remove: false,
                property_id: 1,
                value: vec![1, 2],
                vertex_id: 3,
            },
            IndexPostingMutation::EdgeProperty {
                physical_index_id: PHYSICAL_INDEX_ID,
                remove: true,
                property_id: 2,
                value: vec![4],
                label_id: 5,
                owner_vertex_id: 6,
                slot_index: 7,
            },
            IndexPostingMutation::Label {
                remove: false,
                label_id: 8,
                vertex_id: 9,
            },
        ];
        let bytes = Encode!(&operations).expect("encode posting batch");
        assert_eq!(
            Decode!(&bytes, Vec<IndexPostingMutation>).expect("decode posting batch"),
            operations
        );

        let progress = IndexPostingBatchProgress {
            applied: 2,
            next_index: Some(2),
            instruction_budget_exhausted: true,
        };
        let bytes = Encode!(&progress).expect("encode progress");
        assert_eq!(
            Decode!(&bytes, IndexPostingBatchProgress).expect("decode progress"),
            progress
        );
    }
}
