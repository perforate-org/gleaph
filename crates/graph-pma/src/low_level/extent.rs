//! Extent- and bucket-backed allocator metadata for stable-memory regions.

use candid::CandidType;
use serde::{Deserialize, Serialize};

use super::ids::StableAddr;
use super::region::WasmPages;

/// Identifier for one extent-table entry.
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
    Default,
    Serialize,
    Deserialize,
    CandidType,
)]
pub struct ExtentId {
    pub raw: u32,
}

impl ExtentId {
    /// Null extent-table sentinel.
    pub const NULL: Self = Self { raw: 0 };

    /// Creates one extent-table id.
    pub const fn new(raw: u32) -> Self {
        Self { raw }
    }

    /// Returns whether this id is the null sentinel.
    pub const fn is_null(self) -> bool {
        self.raw == 0
    }
}

/// Contiguous physical span inside stable memory.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct ExtentRef {
    pub addr: StableAddr,
    pub len_bytes: u64,
}

impl ExtentRef {
    /// Creates one physical stable-memory extent reference.
    pub const fn new(addr: StableAddr, len_bytes: u64) -> Self {
        Self { addr, len_bytes }
    }
}

/// Reference to one bucket in a bucket-chain-backed region.
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
    Default,
    Serialize,
    Deserialize,
    CandidType,
)]
pub struct BucketRef {
    pub bucket_id: u32,
}

impl BucketRef {
    /// Creates one bucket reference.
    pub const fn new(bucket_id: u32) -> Self {
        Self { bucket_id }
    }
}

/// Identifier for one bucket-table entry.
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
    Default,
    Serialize,
    Deserialize,
    CandidType,
)]
pub struct BucketId {
    pub raw: u32,
}

impl BucketId {
    /// Null bucket-table sentinel.
    pub const NULL: Self = Self { raw: 0 };

    /// Creates one bucket-table id.
    pub const fn new(raw: u32) -> Self {
        Self { raw }
    }

    /// Returns whether this id is the null sentinel.
    pub const fn is_null(self) -> bool {
        self.raw == 0
    }
}

/// One extent-table node.
///
/// `next` is used both for extent chaining and for free-list chaining in the
/// minimal allocator.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct ExtentHeader {
    pub id: ExtentId,
    pub extent: ExtentRef,
    pub next: ExtentId,
}

impl ExtentHeader {
    /// Creates one extent-table header.
    pub const fn new(id: ExtentId, extent: ExtentRef, next: ExtentId) -> Self {
        Self { id, extent, next }
    }
}

/// Root metadata for an extent-backed region.
///
/// The region manager resolves a [`RegionRef`](crate::low_level::RegionRef)
/// into an `ExtentChain`, then follows `head` / `tail` into the extent table.
///
/// Invariant:
/// - `logical_len_bytes <= allocated_pages.bytes()`
/// - `slack_pages` is allocator slack, not semantic free slots inside PMA
/// - `head` / `tail` are null together for an empty chain
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct ExtentChain {
    pub head: ExtentId,
    pub tail: ExtentId,
    pub logical_len_bytes: u64,
    pub allocated_pages: WasmPages,
    pub slack_pages: WasmPages,
}

impl ExtentChain {
    /// Creates one extent-backed region root.
    pub const fn new(
        head: ExtentId,
        tail: ExtentId,
        logical_len_bytes: u64,
        allocated_pages: WasmPages,
        slack_pages: WasmPages,
    ) -> Self {
        Self {
            head,
            tail,
            logical_len_bytes,
            allocated_pages,
            slack_pages,
        }
    }

    /// Returns whether this chain has no allocated extent.
    pub const fn is_empty(self) -> bool {
        self.head.is_null()
            && self.tail.is_null()
            && self.logical_len_bytes == 0
            && self.allocated_pages.raw == 0
            && self.slack_pages.raw == 0
    }

    /// Chooses a growth path for this extent-backed region without mutating storage.
    pub fn plan_growth(
        self,
        request: ExtentGrowthRequest,
        policy: ExtentGrowthPolicy,
        trailing_region_pages: Option<WasmPages>,
    ) -> ExtentGrowthDecision {
        let requested = request
            .additional_pages
            .raw
            .max(policy.min_append_pages.raw);

        if self.slack_pages.raw >= requested {
            return ExtentGrowthDecision::new(ExtentGrowthKind::InPlace, WasmPages::new(0));
        }

        let shortage = requested - self.slack_pages.raw;

        if let Some(trailing) = trailing_region_pages
            && trailing.raw > 0
            && trailing.raw <= policy.small_region_relocation_max_pages.raw
        {
            return ExtentGrowthDecision::new(
                ExtentGrowthKind::RelocateTrailingSmallRegions,
                WasmPages::new(shortage),
            );
        }

        ExtentGrowthDecision::new(ExtentGrowthKind::RelocateSelf, WasmPages::new(shortage))
    }

    /// Applies a growth decision to extent metadata.
    pub fn apply_growth_decision(
        self,
        request: ExtentGrowthRequest,
        policy: ExtentGrowthPolicy,
        decision: ExtentGrowthDecision,
    ) -> Self {
        let requested = request
            .additional_pages
            .raw
            .max(policy.min_append_pages.raw);

        match decision.growth_kind() {
            ExtentGrowthKind::InPlace => {
                debug_assert!(self.slack_pages.raw >= requested);
                Self {
                    slack_pages: WasmPages::new(self.slack_pages.raw - requested),
                    ..self
                }
            }
            ExtentGrowthKind::RelocateTrailingSmallRegions | ExtentGrowthKind::RelocateSelf => {
                let new_allocated = self.allocated_pages.raw + decision.pages.raw;
                let new_slack = self.slack_pages.raw + decision.pages.raw;

                debug_assert!(new_slack >= requested);

                Self {
                    allocated_pages: WasmPages::new(new_allocated),
                    slack_pages: WasmPages::new(new_slack - requested),
                    ..self
                }
            }
        }
    }
}

/// Chosen growth path for an extent-backed region.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, CandidType)]
pub enum ExtentGrowthKind {
    InPlace = 0,
    RelocateTrailingSmallRegions = 1,
    RelocateSelf = 2,
}

/// Lifecycle state for one edge-storage segment.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, CandidType, Default)]
pub enum EdgeSegmentState {
    Active = 0,
    Retired = 1,
    #[default]
    Free = 2,
}

/// Metadata for one contiguous edge-storage segment.
///
/// A segment is a logical run of `EdgeEntry` slots backed by one extent.
/// `slot_capacity` is expressed in `EdgeEntry` units, not bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct EdgeSegmentHeader {
    pub segment_id: u32,
    pub extent_id: ExtentId,
    pub slot_capacity: u64,
    pub retired_epoch: u64,
    pub state: EdgeSegmentState,
    pub reserved: [u8; 7],
}

impl EdgeSegmentHeader {
    /// Creates one segment-directory entry.
    pub const fn new(
        segment_id: u32,
        extent_id: ExtentId,
        slot_capacity: u64,
        retired_epoch: u64,
        state: EdgeSegmentState,
    ) -> Self {
        Self {
            segment_id,
            extent_id,
            slot_capacity,
            retired_epoch,
            state,
            reserved: [0; 7],
        }
    }

    /// Returns whether this segment can serve traversal reads.
    pub const fn is_active(self) -> bool {
        matches!(self.state, EdgeSegmentState::Active)
    }
}

/// Table of edge-segment headers keyed by segment id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct EdgeSegmentDirectory {
    headers: Vec<EdgeSegmentHeader>,
}

impl EdgeSegmentDirectory {
    /// Inserts or replaces one segment header.
    pub fn insert(&mut self, header: EdgeSegmentHeader) {
        let index = header
            .segment_id
            .checked_sub(1)
            .expect("segment id 0 is reserved as the null sentinel") as usize;
        if index >= self.headers.len() {
            self.headers.resize(index + 1, EdgeSegmentHeader::default());
        }
        self.headers[index] = header;
    }

    /// Returns one segment header by id.
    pub fn get(&self, segment_id: u32) -> Option<&EdgeSegmentHeader> {
        let index = usize::try_from(segment_id.checked_sub(1)?).ok()?;
        let header = self.headers.get(index)?;
        (header.segment_id == segment_id).then_some(header)
    }

    /// Returns one mutable segment header by id.
    pub fn get_mut(&mut self, segment_id: u32) -> Option<&mut EdgeSegmentHeader> {
        let index = usize::try_from(segment_id.checked_sub(1)?).ok()?;
        let header = self.headers.get_mut(index)?;
        (header.segment_id == segment_id).then_some(header)
    }

    /// Returns the next fresh non-zero segment id.
    pub fn next_segment_id(&self) -> u32 {
        (self.headers.len() + 1) as u32
    }

    /// Iterates over initialized segment headers.
    pub fn iter(&self) -> impl Iterator<Item = &EdgeSegmentHeader> {
        self.headers.iter().filter(|header| header.segment_id != 0)
    }
}

/// Policy knobs for extent growth planning.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct ExtentGrowthPolicy {
    pub min_append_pages: WasmPages,
    pub small_region_relocation_max_pages: WasmPages,
}

impl ExtentGrowthPolicy {
    /// Creates a page-based growth policy for extent-backed regions.
    pub const fn new(
        min_append_pages: WasmPages,
        small_region_relocation_max_pages: WasmPages,
    ) -> Self {
        Self {
            min_append_pages,
            small_region_relocation_max_pages,
        }
    }
}

/// Request to grow an extent-backed region.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct ExtentGrowthRequest {
    pub additional_pages: WasmPages,
}

impl ExtentGrowthRequest {
    /// Requests additional allocator capacity in wasm pages.
    pub const fn new(additional_pages: WasmPages) -> Self {
        Self { additional_pages }
    }
}

/// Pure planning result for extent growth.
///
/// This is produced before the allocator mutates any physical stable-memory
/// placement.
///
/// Invariant:
/// - `pages` is expressed in Wasm pages
/// - `RelocateTrailingSmallRegions` refers only to colliding extent-backed
///   trailing regions
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct ExtentGrowthDecision {
    pub kind: u8,
    pub reserved: [u8; 7],
    pub pages: WasmPages,
}

impl ExtentGrowthDecision {
    /// Creates one pure growth decision.
    pub const fn new(kind: ExtentGrowthKind, pages: WasmPages) -> Self {
        Self {
            kind: kind as u8,
            reserved: [0; 7],
            pages,
        }
    }

    /// Returns the chosen growth kind.
    pub fn growth_kind(self) -> ExtentGrowthKind {
        match self.kind {
            0 => ExtentGrowthKind::InPlace,
            1 => ExtentGrowthKind::RelocateTrailingSmallRegions,
            2 => ExtentGrowthKind::RelocateSelf,
            _ => panic!("invalid extent growth kind"),
        }
    }
}

/// Root metadata for a bucket-chain-backed region.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct BucketChain {
    pub head: BucketId,
    pub tail: BucketId,
    pub logical_len_bytes: u64,
}

impl BucketChain {
    /// Creates one bucket-chain-backed region root.
    pub const fn new(head: BucketId, tail: BucketId, logical_len_bytes: u64) -> Self {
        Self {
            head,
            tail,
            logical_len_bytes,
        }
    }
}

/// One bucket-table node.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct BucketHeader {
    pub id: BucketId,
    pub addr: StableAddr,
    pub next: BucketId,
    pub reserved: u32,
}

impl BucketHeader {
    /// Creates one bucket-table header.
    pub const fn new(id: BucketId, addr: StableAddr, next: BucketId) -> Self {
        Self {
            id,
            addr,
            next,
            reserved: 0,
        }
    }
}

/// Free-list head for reusable extent ids.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct FreeExtentList {
    pub head: ExtentId,
}

impl FreeExtentList {
    /// Creates a free-list root with an explicit head id.
    pub const fn new(head: ExtentId) -> Self {
        Self { head }
    }

    /// Returns whether the free list is empty.
    pub const fn is_empty(self) -> bool {
        self.head.is_null()
    }

    /// Pops one free extent id from the front of the list.
    pub fn pop(&mut self) -> Option<ExtentId> {
        if self.head.is_null() {
            None
        } else {
            let id = self.head;
            self.head = ExtentId::NULL;
            Some(id)
        }
    }

    /// Pushes one extent id onto the front of the free list.
    pub fn push(&mut self, id: ExtentId) {
        self.head = id;
    }
}

/// Free-list head for reusable bucket ids.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct FreeBucketList {
    pub head: BucketId,
}

impl FreeBucketList {
    /// Creates a free-list root with an explicit head id.
    pub const fn new(head: BucketId) -> Self {
        Self { head }
    }

    /// Returns whether the free list is empty.
    pub const fn is_empty(self) -> bool {
        self.head.is_null()
    }

    /// Pops one free bucket id from the front of the list.
    pub fn pop(&mut self) -> Option<BucketId> {
        if self.head.is_null() {
            None
        } else {
            let id = self.head;
            self.head = BucketId::NULL;
            Some(id)
        }
    }

    /// Pushes one bucket id onto the front of the free list.
    pub fn push(&mut self, id: BucketId) {
        self.head = id;
    }
}

/// Table of extent headers keyed by [`ExtentId`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ExtentTable {
    headers: Vec<ExtentHeader>,
}

impl ExtentTable {
    /// Inserts or replaces one extent header by id.
    pub fn insert(&mut self, header: ExtentHeader) {
        let idx = header.id.raw as usize;
        if idx == 0 {
            panic!("extent id 0 is reserved as null");
        }
        if idx > self.headers.len() {
            self.headers.push(header);
        } else {
            self.headers[idx - 1] = header;
        }
    }

    /// Returns an immutable view of one extent header.
    pub fn get(&self, id: ExtentId) -> Option<&ExtentHeader> {
        self.headers.iter().find(|header| header.id == id)
    }

    /// Returns a mutable view of one extent header.
    pub fn get_mut(&mut self, id: ExtentId) -> Option<&mut ExtentHeader> {
        self.headers.iter_mut().find(|header| header.id == id)
    }

    /// Returns the number of stored extent headers.
    pub fn len(&self) -> usize {
        self.headers.len()
    }

    /// Returns true when no extent headers are stored.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Returns the next append-only extent id when no free id is reused.
    pub fn next_id(&self) -> ExtentId {
        ExtentId::new((self.headers.len() + 1) as u32)
    }
}

/// Table of bucket headers keyed by [`BucketId`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct BucketTable {
    headers: Vec<BucketHeader>,
}

impl BucketTable {
    /// Inserts or replaces one bucket header by id.
    pub fn insert(&mut self, header: BucketHeader) {
        let idx = header.id.raw as usize;
        if idx == 0 {
            panic!("bucket id 0 is reserved as null");
        }
        if idx > self.headers.len() {
            self.headers.push(header);
        } else {
            self.headers[idx - 1] = header;
        }
    }

    /// Returns an immutable view of one bucket header.
    pub fn get(&self, id: BucketId) -> Option<&BucketHeader> {
        self.headers.iter().find(|header| header.id == id)
    }

    /// Returns a mutable view of one bucket header.
    pub fn get_mut(&mut self, id: BucketId) -> Option<&mut BucketHeader> {
        self.headers.iter_mut().find(|header| header.id == id)
    }

    /// Returns the number of stored bucket headers.
    pub fn len(&self) -> usize {
        self.headers.len()
    }

    /// Returns true when no bucket headers are stored.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty()
    }

    /// Returns the next append-only bucket id when no free id is reused.
    pub fn next_id(&self) -> BucketId {
        BucketId::new((self.headers.len() + 1) as u32)
    }
}

const _: [(); 4] = [(); core::mem::size_of::<ExtentId>()];
const _: [(); 16] = [(); core::mem::size_of::<ExtentRef>()];
const _: [(); 4] = [(); core::mem::size_of::<BucketRef>()];
const _: [(); 4] = [(); core::mem::size_of::<BucketId>()];
const _: [(); 32] = [(); core::mem::size_of::<ExtentHeader>()];
const _: [(); 32] = [(); core::mem::size_of::<ExtentChain>()];
const _: [(); 16] = [(); core::mem::size_of::<ExtentGrowthPolicy>()];
const _: [(); 8] = [(); core::mem::size_of::<ExtentGrowthRequest>()];
const _: [(); 16] = [(); core::mem::size_of::<ExtentGrowthDecision>()];
const _: [(); 16] = [(); core::mem::size_of::<BucketChain>()];
const _: [(); 24] = [(); core::mem::size_of::<BucketHeader>()];
const _: [(); 4] = [(); core::mem::size_of::<FreeExtentList>()];
const _: [(); 4] = [(); core::mem::size_of::<FreeBucketList>()];

#[cfg(test)]
mod tests {
    use super::{
        BucketChain, BucketHeader, BucketId, BucketRef, BucketTable, ExtentChain,
        ExtentGrowthDecision, ExtentGrowthKind, ExtentGrowthPolicy, ExtentGrowthRequest,
        ExtentHeader, ExtentId, ExtentRef, ExtentTable, FreeBucketList, FreeExtentList,
    };
    use crate::low_level::{StableAddr, WasmPages};

    #[test]
    fn extent_root_metadata_has_stable_shape() {
        let extent = ExtentRef::new(StableAddr(8192), 16384);
        let header = ExtentHeader::new(ExtentId::new(1), extent, ExtentId::new(9));
        let chain = ExtentChain::new(
            ExtentId::new(1),
            ExtentId::new(9),
            12000,
            WasmPages::new(8),
            WasmPages::new(2),
        );

        assert_eq!(header.extent.addr.0, 8192);
        assert_eq!(header.next, ExtentId::new(9));
        assert_eq!(chain.head, ExtentId::new(1));
        assert_eq!(chain.tail, ExtentId::new(9));
        assert_eq!(chain.logical_len_bytes, 12000);
        assert_eq!(chain.allocated_pages.raw, 8);
        assert_eq!(chain.slack_pages.raw, 2);
        assert!(!chain.is_empty());
    }

    #[test]
    fn bucket_chain_root_metadata_has_stable_shape() {
        let bucket = BucketRef::new(3);
        let chain = BucketChain::new(BucketId::new(bucket.bucket_id), BucketId::new(12), 4096);
        assert_eq!(chain.head, BucketId::new(3));
        assert_eq!(chain.tail, BucketId::new(12));
        assert_eq!(chain.logical_len_bytes, 4096);
    }

    #[test]
    fn extent_growth_types_capture_page_based_policy() {
        let policy = ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8));
        let request = ExtentGrowthRequest::new(WasmPages::new(4));
        let decision = ExtentGrowthDecision::new(
            ExtentGrowthKind::RelocateTrailingSmallRegions,
            request.additional_pages,
        );

        assert_eq!(policy.min_append_pages.raw, 2);
        assert_eq!(policy.small_region_relocation_max_pages.raw, 8);
        assert_eq!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        assert_eq!(decision.pages.raw, 4);
    }

    #[test]
    fn growth_prefers_in_place_when_slack_is_enough() {
        let chain = ExtentChain::new(
            ExtentId::new(1),
            ExtentId::new(1),
            4096,
            WasmPages::new(8),
            WasmPages::new(6),
        );
        let decision = chain.plan_growth(
            ExtentGrowthRequest::new(WasmPages::new(4)),
            ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8)),
            None,
        );

        assert_eq!(decision.growth_kind(), ExtentGrowthKind::InPlace);
        assert_eq!(decision.pages.raw, 0);
    }

    #[test]
    fn growth_can_relocate_small_trailing_regions() {
        let chain = ExtentChain::new(
            ExtentId::new(1),
            ExtentId::new(2),
            4096,
            WasmPages::new(8),
            WasmPages::new(1),
        );
        let decision = chain.plan_growth(
            ExtentGrowthRequest::new(WasmPages::new(4)),
            ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8)),
            Some(WasmPages::new(3)),
        );

        assert_eq!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        assert_eq!(decision.pages.raw, 3);
    }

    #[test]
    fn growth_relocates_self_when_trailing_region_is_too_large() {
        let chain = ExtentChain::new(
            ExtentId::new(1),
            ExtentId::new(2),
            4096,
            WasmPages::new(8),
            WasmPages::new(1),
        );
        let decision = chain.plan_growth(
            ExtentGrowthRequest::new(WasmPages::new(4)),
            ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8)),
            Some(WasmPages::new(12)),
        );

        assert_eq!(decision.growth_kind(), ExtentGrowthKind::RelocateSelf);
        assert_eq!(decision.pages.raw, 3);
    }

    #[test]
    fn applying_in_place_growth_consumes_slack_only() {
        let chain = ExtentChain::new(
            ExtentId::new(1),
            ExtentId::new(1),
            4096,
            WasmPages::new(8),
            WasmPages::new(6),
        );
        let policy = ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8));
        let request = ExtentGrowthRequest::new(WasmPages::new(4));
        let decision = chain.plan_growth(request, policy, None);
        let updated = chain.apply_growth_decision(request, policy, decision);

        assert_eq!(updated.allocated_pages.raw, 8);
        assert_eq!(updated.slack_pages.raw, 2);
    }

    #[test]
    fn applying_relocation_growth_increases_allocation_before_consuming() {
        let chain = ExtentChain::new(
            ExtentId::new(1),
            ExtentId::new(2),
            4096,
            WasmPages::new(8),
            WasmPages::new(1),
        );
        let policy = ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8));
        let request = ExtentGrowthRequest::new(WasmPages::new(4));
        let decision = chain.plan_growth(request, policy, Some(WasmPages::new(3)));
        let updated = chain.apply_growth_decision(request, policy, decision);

        assert_eq!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        assert_eq!(updated.allocated_pages.raw, 11);
        assert_eq!(updated.slack_pages.raw, 0);
    }

    #[test]
    fn free_extent_list_uses_null_extent_id() {
        let empty = FreeExtentList::new(ExtentId::NULL);
        let non_empty = FreeExtentList::new(ExtentId::new(4));
        assert!(empty.is_empty());
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn free_extent_list_can_push_and_pop() {
        let mut free = FreeExtentList::default();
        free.push(ExtentId::new(7));
        assert_eq!(free.pop(), Some(ExtentId::new(7)));
        assert_eq!(free.pop(), None);
    }

    #[test]
    fn extent_table_tracks_headers_by_id() {
        let mut table = ExtentTable::default();
        table.insert(ExtentHeader::new(
            ExtentId::new(3),
            ExtentRef::new(StableAddr(4096), 2048),
            ExtentId::NULL,
        ));

        assert_eq!(table.len(), 1);
        assert_eq!(
            table
                .get(ExtentId::new(3))
                .map(|header| header.extent.len_bytes),
            Some(2048)
        );
        assert!(table.get(ExtentId::new(4)).is_none());
    }

    #[test]
    fn free_bucket_list_uses_null_bucket_id() {
        let empty = FreeBucketList::new(BucketId::NULL);
        let non_empty = FreeBucketList::new(BucketId::new(5));
        assert!(empty.is_empty());
        assert!(!non_empty.is_empty());
    }

    #[test]
    fn free_bucket_list_can_push_and_pop() {
        let mut free = FreeBucketList::default();
        free.push(BucketId::new(6));
        assert_eq!(free.pop(), Some(BucketId::new(6)));
        assert_eq!(free.pop(), None);
    }

    #[test]
    fn bucket_table_tracks_headers_by_id() {
        let mut table = BucketTable::default();
        table.insert(BucketHeader::new(
            BucketId::new(2),
            StableAddr(4096),
            BucketId::new(8),
        ));

        assert_eq!(table.len(), 1);
        assert_eq!(
            table.get(BucketId::new(2)).map(|header| header.next),
            Some(BucketId::new(8))
        );
        assert_eq!(
            table.get(BucketId::new(2)).map(|header| header.addr),
            Some(StableAddr(4096))
        );
        assert!(table.get(BucketId::new(3)).is_none());
    }
}
