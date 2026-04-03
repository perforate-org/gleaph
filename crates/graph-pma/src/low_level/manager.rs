//! Region-manager metadata and allocator-side state transitions.

use candid::CandidType;
use serde::{Deserialize, Serialize};

use super::extent::{
    BucketChain, BucketHeader, BucketId, BucketTable, EdgeSegmentDirectory, EdgeSegmentHeader,
    EdgeSegmentState, ExtentChain, ExtentGrowthDecision, ExtentGrowthKind, ExtentGrowthPolicy,
    ExtentGrowthRequest, ExtentHeader, ExtentId, ExtentRef, ExtentTable, FreeBucketList,
    FreeExtentList,
};
use super::ids::{EdgeRef, StableAddr};
use super::region::{
    BucketSizeInPages, RegionKind, RegionManagerLayout, RegionRef, RegionStorageKind, WasmPages,
};

/// Metadata-only region manager for stable-memory layout and extents.
///
/// This owns region directory state, extent/bucket allocator metadata, and the
/// first pure growth / relocation rules. It does not yet perform stable-memory
/// IO.
///
/// Invariant:
/// - region directory metadata is authoritative for which tenants exist
/// - extent/bucket allocator metadata may change without changing adjacency
///   semantics
/// - physical address reuse must not change surface-local indexes
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RegionManager {
    pub layout: RegionManagerLayout,
    pub extent_table: ExtentTable,
    pub free_extents: FreeExtentList,
    pub bucket_table: BucketTable,
    pub free_buckets: FreeBucketList,
    pub forward_edge_segments: EdgeSegmentDirectory,
    pub reverse_edge_segments: EdgeSegmentDirectory,
    pub next_extent_addr: StableAddr,
    free_extent_addrs: Vec<ExtentRef>,
    extent_chains: Vec<ExtentChain>,
    bucket_chains: Vec<BucketChain>,
}

impl RegionManager {
    const EDGE_ENTRY_LEN_BYTES: u64 = 8;

    /// Creates an empty region manager with the chosen bucket granularity.
    pub fn with_bucket_size(bucket_size_in_pages: BucketSizeInPages) -> Self {
        Self {
            layout: RegionManagerLayout::with_bucket_size(bucket_size_in_pages),
            extent_table: ExtentTable::default(),
            free_extents: FreeExtentList::default(),
            bucket_table: BucketTable::default(),
            free_buckets: FreeBucketList::default(),
            forward_edge_segments: EdgeSegmentDirectory::default(),
            reverse_edge_segments: EdgeSegmentDirectory::default(),
            next_extent_addr: StableAddr(0),
            free_extent_addrs: Vec::new(),
            extent_chains: Vec::new(),
            bucket_chains: Vec::new(),
        }
    }

    /// Defines one extent-backed region and allocates its initial physical extent.
    pub fn define_extent_region(&mut self, kind: RegionKind, chain: ExtentChain) -> RegionRef {
        let extent_id = self.allocate_extent_id();
        let extent = ExtentRef::new(
            self.allocate_extent_addr(chain.allocated_pages.bytes()),
            chain.allocated_pages.bytes(),
        );
        self.extent_table
            .insert(ExtentHeader::new(extent_id, extent, ExtentId::NULL));

        let root = (self.extent_chains.len() + 1) as u32;
        let stored_chain = ExtentChain::new(
            extent_id,
            extent_id,
            chain.logical_len_bytes,
            chain.allocated_pages,
            chain.slack_pages,
        );
        self.extent_chains.push(stored_chain);
        let region = RegionRef::new(
            RegionStorageKind::Extent,
            kind,
            root,
            stored_chain.logical_len_bytes,
        );
        self.layout.define_region(region);
        region
    }

    /// Defines one bucket-chain-backed region and allocates its initial root bucket.
    pub fn define_bucket_region(&mut self, kind: RegionKind, chain: BucketChain) -> RegionRef {
        let bucket_id = self.allocate_bucket_id();
        let bucket_addr = self.allocate_bucket_addr();
        self.bucket_table
            .insert(BucketHeader::new(bucket_id, bucket_addr, BucketId::NULL));

        let root = (self.bucket_chains.len() + 1) as u32;
        let stored_chain = BucketChain::new(bucket_id, bucket_id, chain.logical_len_bytes);
        self.bucket_chains.push(stored_chain);
        let region = RegionRef::new(
            RegionStorageKind::BucketChain,
            kind,
            root,
            stored_chain.logical_len_bytes,
        );
        self.layout.define_region(region);
        region
    }

    /// Returns the current extent-chain metadata for the requested region kind.
    pub fn extent_chain(&self, kind: RegionKind) -> Option<ExtentChain> {
        let region = self.layout.region(kind)?;
        if region.storage_kind() != RegionStorageKind::Extent || region.root == 0 {
            return None;
        }
        self.extent_chains.get((region.root - 1) as usize).copied()
    }

    /// Returns the current bucket-chain metadata for the requested region kind.
    pub fn bucket_chain(&self, kind: RegionKind) -> Option<BucketChain> {
        let region = self.layout.region(kind)?;
        if region.storage_kind() != RegionStorageKind::BucketChain || region.root == 0 {
            return None;
        }
        self.bucket_chains.get((region.root - 1) as usize).copied()
    }

    /// Returns the edge-segment directory attached to one edge-entry region.
    pub fn edge_segment_directory(&self, kind: RegionKind) -> Option<&EdgeSegmentDirectory> {
        match kind {
            RegionKind::ForwardEdgeEntries => Some(&self.forward_edge_segments),
            RegionKind::ReverseEdgeEntries => Some(&self.reverse_edge_segments),
            _ => None,
        }
    }

    /// Returns a mutable edge-segment directory attached to one edge-entry region.
    pub fn edge_segment_directory_mut(
        &mut self,
        kind: RegionKind,
    ) -> Option<&mut EdgeSegmentDirectory> {
        match kind {
            RegionKind::ForwardEdgeEntries => Some(&mut self.forward_edge_segments),
            RegionKind::ReverseEdgeEntries => Some(&mut self.reverse_edge_segments),
            _ => None,
        }
    }

    /// Inserts or replaces one edge-segment header for the given edge-entry region.
    pub fn register_edge_segment(
        &mut self,
        kind: RegionKind,
        header: EdgeSegmentHeader,
    ) -> Option<()> {
        self.edge_segment_directory_mut(kind)?.insert(header);
        Some(())
    }

    /// Allocates one new explicit edge segment backed by its own extent.
    pub fn allocate_edge_segment(
        &mut self,
        kind: RegionKind,
        slot_capacity: u64,
        state: EdgeSegmentState,
    ) -> Option<EdgeSegmentHeader> {
        let region = self.layout.region(kind)?;
        if region.storage_kind() != RegionStorageKind::Extent {
            return None;
        }
        let segment_id = self.next_edge_segment_id(kind)?;
        let len_bytes = slot_capacity.checked_mul(Self::EDGE_ENTRY_LEN_BYTES)?;
        let extent_id = self.allocate_extent_id();
        let extent = ExtentRef::new(self.allocate_extent_addr(len_bytes), len_bytes);
        self.extent_table
            .insert(ExtentHeader::new(extent_id, extent, ExtentId::NULL));
        let header = EdgeSegmentHeader::new(segment_id, extent_id, slot_capacity, 0, state);
        self.register_edge_segment(kind, header)?;
        Some(header)
    }

    /// Allocates a fresh active edge segment and retires the replaced segment.
    pub fn replace_edge_segment_for_maintenance(
        &mut self,
        kind: RegionKind,
        replaced_segment_id: u32,
        new_slot_capacity: u64,
        retired_epoch: u64,
    ) -> Option<(EdgeSegmentHeader, Option<EdgeSegmentHeader>)> {
        let new_segment =
            self.allocate_edge_segment(kind, new_slot_capacity, EdgeSegmentState::Active)?;
        let replaced = if replaced_segment_id == 0 {
            None
        } else {
            self.retire_edge_segment(kind, replaced_segment_id, retired_epoch)?;
            self.edge_segment(kind, replaced_segment_id)
        };
        Some((new_segment, replaced))
    }

    /// Returns the next fresh non-zero segment id for the given edge-entry region.
    pub fn next_edge_segment_id(&self, kind: RegionKind) -> Option<u32> {
        Some(self.edge_segment_directory(kind)?.next_segment_id())
    }

    /// Marks one explicit edge segment as retired at the given GC epoch.
    pub fn retire_edge_segment(
        &mut self,
        kind: RegionKind,
        segment_id: u32,
        retired_epoch: u64,
    ) -> Option<EdgeSegmentHeader> {
        if segment_id == 0 {
            return None;
        }
        let header = self.edge_segment_directory_mut(kind)?.get_mut(segment_id)?;
        let previous = *header;
        header.state = EdgeSegmentState::Retired;
        header.retired_epoch = retired_epoch;
        Some(previous)
    }

    /// Reactivates one explicit edge segment, clearing any retired epoch.
    pub fn reactivate_edge_segment(
        &mut self,
        kind: RegionKind,
        segment_id: u32,
    ) -> Option<EdgeSegmentHeader> {
        if segment_id == 0 {
            return None;
        }
        let header = self.edge_segment_directory_mut(kind)?.get_mut(segment_id)?;
        let previous = *header;
        header.state = EdgeSegmentState::Active;
        header.retired_epoch = 0;
        Some(previous)
    }

    /// Marks one explicit edge segment as reusable.
    pub fn free_edge_segment(
        &mut self,
        kind: RegionKind,
        segment_id: u32,
    ) -> Option<EdgeSegmentHeader> {
        if segment_id == 0 {
            return None;
        }
        let header = self.edge_segment_directory_mut(kind)?.get_mut(segment_id)?;
        let previous = *header;
        header.state = EdgeSegmentState::Free;
        Some(previous)
    }

    /// Reclaims the extent backing one explicit free/retired edge segment.
    pub fn reclaim_edge_segment_storage(
        &mut self,
        kind: RegionKind,
        segment_id: u32,
    ) -> Option<EdgeSegmentHeader> {
        if segment_id == 0 {
            return None;
        }
        let header = self.edge_segment(kind, segment_id)?;
        if header.state == EdgeSegmentState::Active {
            return None;
        }
        let extent = self.extent_table.get(header.extent_id)?.extent;
        self.free_extent_addr(extent);
        self.release_extent_id_internal(header.extent_id);
        Some(header)
    }

    /// Returns one segment header by id for the given edge-entry region.
    ///
    /// Segment id 0 is the root flat edge-index extent for single-segment layouts.
    pub fn edge_segment(&self, kind: RegionKind, segment_id: u32) -> Option<EdgeSegmentHeader> {
        if segment_id == 0 {
            let region = self.layout.region(kind)?;
            let slot_capacity = region.logical_len_bytes / 8;
            return Some(EdgeSegmentHeader::new(
                segment_id,
                ExtentId::NULL,
                slot_capacity,
                0,
                EdgeSegmentState::Active,
            ));
        }
        self.edge_segment_directory(kind)?.get(segment_id).copied()
    }

    /// Returns explicit segments eligible for retirement sweep at `current_epoch`.
    pub fn retired_edge_segments_eligible_for_sweep(
        &self,
        kind: RegionKind,
        current_epoch: u64,
        min_retired_epochs: u64,
    ) -> Option<Vec<EdgeSegmentHeader>> {
        let directory = self.edge_segment_directory(kind)?;
        Some(
            directory
                .iter()
                .copied()
                .filter(|header| {
                    header.state == EdgeSegmentState::Retired
                        && current_epoch.saturating_sub(header.retired_epoch) >= min_retired_epochs
                })
                .collect(),
        )
    }

    /// Frees and reclaims retired explicit segments eligible at `current_epoch`.
    pub fn sweep_retired_edge_segments(
        &mut self,
        kind: RegionKind,
        current_epoch: u64,
        min_retired_epochs: u64,
    ) -> Option<Vec<EdgeSegmentHeader>> {
        let candidates =
            self.retired_edge_segments_eligible_for_sweep(kind, current_epoch, min_retired_epochs)?;
        let mut reclaimed = Vec::with_capacity(candidates.len());
        for header in candidates {
            self.free_edge_segment(kind, header.segment_id)?;
            reclaimed.push(self.reclaim_edge_segment_storage(kind, header.segment_id)?);
        }
        Some(reclaimed)
    }

    /// Returns the segment slot capacity for one edge ref inside the given edge-entry region.
    pub fn edge_ref_slot_capacity(&self, kind: RegionKind, edge_ref: EdgeRef) -> Option<u64> {
        self.edge_segment(kind, edge_ref.segment_id())
            .map(|header| header.slot_capacity)
    }

    /// Resolves one edge ref into a physical extent and segment metadata.
    pub fn resolve_edge_ref(
        &self,
        kind: RegionKind,
        edge_ref: EdgeRef,
    ) -> Option<(EdgeSegmentHeader, ExtentRef)> {
        let header = self.edge_segment(kind, edge_ref.segment_id())?;
        if edge_ref.segment_id() == 0 {
            return Some((header, self.region_extent(kind)?));
        }
        let extent = self.extent_table.get(header.extent_id)?.extent;
        Some((header, extent))
    }

    /// Resolves one bucket header for a bucket-backed region.
    pub fn bucket_header(&self, bucket_id: BucketId) -> Option<&BucketHeader> {
        self.bucket_table.get(bucket_id)
    }

    /// Returns the physical bucket size in bytes configured for this manager.
    pub fn bucket_size_bytes(&self) -> u64 {
        self.layout.bucket_size_in_pages.bytes()
    }

    /// Ensures one bucket-backed region has enough buckets for `logical_len_bytes`.
    pub fn ensure_bucket_region_capacity(
        &mut self,
        kind: RegionKind,
        logical_len_bytes: u64,
    ) -> Option<BucketChain> {
        let region = self.layout.region(kind)?;
        if region.storage_kind() != RegionStorageKind::BucketChain || region.root == 0 {
            return None;
        }
        let idx = (region.root - 1) as usize;
        let bucket_size_bytes = self.bucket_size_bytes();
        let required_buckets = logical_len_bytes.max(1).div_ceil(bucket_size_bytes) as usize;
        let mut chain = *self.bucket_chains.get(idx)?;
        let mut current_buckets = self.count_bucket_chain(chain);
        while current_buckets < required_buckets {
            let new_bucket_id = self.allocate_bucket_id();
            let new_header =
                BucketHeader::new(new_bucket_id, self.allocate_bucket_addr(), BucketId::NULL);
            self.bucket_table.insert(new_header);
            if chain.tail.is_null() {
                chain.head = new_bucket_id;
                chain.tail = new_bucket_id;
            } else if let Some(tail) = self.bucket_table.get_mut(chain.tail) {
                tail.next = new_bucket_id;
                chain.tail = new_bucket_id;
            } else {
                return None;
            }
            current_buckets += 1;
        }
        chain.logical_len_bytes = logical_len_bytes;
        self.bucket_chains[idx] = chain;
        self.layout.define_region(RegionRef::new(
            RegionStorageKind::BucketChain,
            kind,
            region.root,
            logical_len_bytes,
        ));
        Some(chain)
    }

    /// Resolves the head physical extent for one extent-backed region.
    pub fn region_extent(&self, kind: RegionKind) -> Option<ExtentRef> {
        let chain = self.extent_chain(kind)?;
        self.extent_table
            .get(chain.head)
            .map(|header| header.extent)
    }

    /// Updates only the logical payload length recorded for a region.
    pub fn set_region_logical_len(
        &mut self,
        kind: RegionKind,
        logical_len_bytes: u64,
    ) -> Option<()> {
        let region = self.layout.region(kind)?;
        self.layout.define_region(RegionRef::new(
            region.storage_kind(),
            kind,
            region.root,
            logical_len_bytes,
        ));

        match region.storage_kind() {
            RegionStorageKind::Extent => {
                let idx = (region.root.checked_sub(1)?) as usize;
                let chain = self.extent_chains.get_mut(idx)?;
                chain.logical_len_bytes = logical_len_bytes;
            }
            RegionStorageKind::BucketChain => {
                let idx = (region.root.checked_sub(1)?) as usize;
                let chain = self.bucket_chains.get_mut(idx)?;
                chain.logical_len_bytes = logical_len_bytes;
            }
        }

        Some(())
    }

    /// Computes a pure growth decision for one extent-backed region.
    pub fn plan_extent_growth(
        &self,
        kind: RegionKind,
        request: ExtentGrowthRequest,
        policy: ExtentGrowthPolicy,
    ) -> Option<ExtentGrowthDecision> {
        let chain = self.extent_chain(kind)?;
        let requested = request
            .additional_pages
            .raw
            .max(policy.min_append_pages.raw);
        let shortage = requested.saturating_sub(chain.slack_pages.raw);
        let trailing_region_pages = if shortage == 0 {
            None
        } else {
            let colliding_pages =
                self.colliding_trailing_extent_region_pages(kind, WasmPages::new(shortage));
            if colliding_pages.raw == 0 {
                None
            } else {
                Some(colliding_pages)
            }
        };
        Some(chain.plan_growth(request, policy, trailing_region_pages))
    }

    /// Applies one growth decision to region-manager metadata.
    pub fn apply_extent_growth(
        &mut self,
        kind: RegionKind,
        request: ExtentGrowthRequest,
        policy: ExtentGrowthPolicy,
        decision: ExtentGrowthDecision,
    ) -> Option<ExtentChain> {
        let trailing_kinds = if matches!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        ) {
            self.colliding_trailing_extent_regions(kind, decision.pages)
        } else {
            Vec::new()
        };
        self.apply_extent_growth_with_trailing(kind, request, policy, decision, &trailing_kinds)
    }

    /// Applies one growth decision while forcing a specific set of trailing regions to relocate.
    pub fn apply_extent_growth_with_trailing(
        &mut self,
        kind: RegionKind,
        request: ExtentGrowthRequest,
        policy: ExtentGrowthPolicy,
        decision: ExtentGrowthDecision,
        trailing_kinds: &[RegionKind],
    ) -> Option<ExtentChain> {
        let region = self.layout.region(kind)?;
        if region.storage_kind() != RegionStorageKind::Extent || region.root == 0 {
            return None;
        }
        let idx = (region.root - 1) as usize;
        let current = *self.extent_chains.get(idx)?;
        let updated = current.apply_growth_decision(request, policy, decision);
        self.apply_extent_header_growth(updated.head, current, updated, decision, trailing_kinds);
        self.extent_chains[idx] = updated;
        self.layout.define_region(RegionRef::new(
            RegionStorageKind::Extent,
            kind,
            region.root,
            updated.logical_len_bytes,
        ));
        Some(updated)
    }

    fn apply_extent_header_growth(
        &mut self,
        head: ExtentId,
        previous: ExtentChain,
        updated: ExtentChain,
        decision: ExtentGrowthDecision,
        trailing_kinds: &[RegionKind],
    ) {
        let relocated_addr = match decision.growth_kind() {
            ExtentGrowthKind::RelocateSelf => {
                Some(self.allocate_extent_addr(updated.allocated_pages.bytes()))
            }
            _ => None,
        };
        let should_relocate_trailing = matches!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        let mut freed_extent = None;

        {
            let Some(header) = self.extent_table.get_mut(head) else {
                return;
            };

            match decision.growth_kind() {
                ExtentGrowthKind::InPlace => {
                    header.extent.len_bytes = updated.allocated_pages.bytes();
                }
                ExtentGrowthKind::RelocateTrailingSmallRegions => {
                    header.extent.len_bytes = updated.allocated_pages.bytes();
                }
                ExtentGrowthKind::RelocateSelf => {
                    let new_addr =
                        relocated_addr.expect("relocate-self should allocate an address");
                    let old_extent = header.extent;
                    header.extent.addr = new_addr;
                    header.extent.len_bytes = updated.allocated_pages.bytes();
                    freed_extent = Some(old_extent);
                    debug_assert_ne!(new_addr, old_extent.addr);
                    debug_assert!(updated.allocated_pages.raw >= previous.allocated_pages.raw);
                }
            }
        }

        if let Some(old_extent) = freed_extent {
            self.free_extent_addr(old_extent);
        }

        if should_relocate_trailing {
            for &kind in trailing_kinds {
                self.relocate_extent_region(kind);
            }
        }
    }

    fn relocate_extent_region(&mut self, kind: RegionKind) {
        let Some(region) = self.layout.region(kind) else {
            return;
        };
        if region.storage_kind() != RegionStorageKind::Extent || region.root == 0 {
            return;
        }
        let Some(chain) = self.extent_chain(kind) else {
            return;
        };
        let new_addr = {
            let len_bytes = self
                .extent_table
                .get(chain.head)
                .map(|header| header.extent.len_bytes)
                .unwrap_or(0);
            self.allocate_extent_addr(len_bytes)
        };
        let Some(header) = self.extent_table.get_mut(chain.head) else {
            return;
        };
        let old_extent = header.extent;
        header.extent.addr = new_addr;
        self.free_extent_addr(old_extent);
    }

    #[cfg(test)]
    fn next_extent_region_after(&self, kind: RegionKind) -> Option<RegionKind> {
        let current_region = self.layout.region(kind)?;
        if current_region.storage_kind() != RegionStorageKind::Extent || current_region.root == 0 {
            return None;
        }
        let current_chain = self.extent_chain(kind)?;
        let current_end_addr = self
            .extent_table
            .get(current_chain.head)
            .map(|header| header.extent.addr.0 + header.extent.len_bytes)?;

        self.layout
            .directory
            .iter()
            .filter_map(|entry| {
                let region = entry.region;
                if region.storage_kind() != RegionStorageKind::Extent {
                    return None;
                }
                let candidate_kind = region.region_kind();
                if candidate_kind == kind || region.root == 0 {
                    return None;
                }
                let chain = self.extent_chain(candidate_kind)?;
                let header = self.extent_table.get(chain.head)?;
                Some((candidate_kind, header.extent.addr.0))
            })
            .filter(|(_, addr)| *addr >= current_end_addr)
            .min_by_key(|(_, addr)| *addr)
            .map(|(candidate_kind, _)| candidate_kind)
    }

    fn colliding_trailing_extent_regions(
        &self,
        kind: RegionKind,
        additional_pages: super::region::WasmPages,
    ) -> Vec<RegionKind> {
        let Some(current_region) = self.layout.region(kind) else {
            return Vec::new();
        };
        if current_region.storage_kind() != RegionStorageKind::Extent || current_region.root == 0 {
            return Vec::new();
        }
        let Some(current_chain) = self.extent_chain(kind) else {
            return Vec::new();
        };
        let Some(current_header) = self.extent_table.get(current_chain.head) else {
            return Vec::new();
        };

        let current_end = current_header.extent.addr.0 + current_header.extent.len_bytes;
        let expanded_end = current_end + additional_pages.bytes();

        let mut candidates = self
            .layout
            .directory
            .iter()
            .filter_map(|entry| {
                let region = entry.region;
                if region.storage_kind() != RegionStorageKind::Extent {
                    return None;
                }
                let candidate_kind = region.region_kind();
                if candidate_kind == kind || region.root == 0 {
                    return None;
                }
                let chain = self.extent_chain(candidate_kind)?;
                let header = self.extent_table.get(chain.head)?;
                let start = header.extent.addr.0;
                Some((candidate_kind, start))
            })
            .filter(|(_, start)| *start >= current_end && *start < expanded_end)
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, start)| *start);
        candidates.into_iter().map(|(kind, _)| kind).collect()
    }

    fn colliding_trailing_extent_region_pages(
        &self,
        kind: RegionKind,
        additional_pages: super::region::WasmPages,
    ) -> super::region::WasmPages {
        let mut total = 0_u64;
        for trailing_kind in self.colliding_trailing_extent_regions(kind, additional_pages) {
            if let Some(chain) = self.extent_chain(trailing_kind) {
                total = total.saturating_add(chain.allocated_pages.raw);
            }
        }
        super::region::WasmPages::new(total)
    }

    fn allocate_extent_id(&mut self) -> ExtentId {
        let head = self.free_extents.head;
        if head.is_null() {
            return self.extent_table.next_id();
        }

        let next = self
            .extent_table
            .get(head)
            .map(|header| header.next)
            .unwrap_or(ExtentId::NULL);
        self.free_extents.head = next;
        if let Some(header) = self.extent_table.get_mut(head) {
            header.next = ExtentId::NULL;
        }
        head
    }

    fn allocate_extent_addr(&mut self, len_bytes: u64) -> StableAddr {
        if let Some((idx, free)) = self
            .free_extent_addrs
            .iter()
            .copied()
            .enumerate()
            .find(|(_, free)| free.len_bytes >= len_bytes)
        {
            let addr = free.addr;
            if free.len_bytes == len_bytes {
                self.free_extent_addrs.remove(idx);
            } else {
                self.free_extent_addrs[idx] = ExtentRef::new(
                    StableAddr(free.addr.0 + len_bytes),
                    free.len_bytes - len_bytes,
                );
            }
            return addr;
        }

        let addr = self.next_extent_addr;
        self.next_extent_addr = StableAddr(self.next_extent_addr.0 + len_bytes);
        addr
    }

    fn free_extent_addr(&mut self, extent: ExtentRef) {
        if extent.len_bytes == 0 {
            return;
        }
        self.free_extent_addrs.push(extent);
        self.free_extent_addrs.sort_by_key(|free| free.addr.0);

        let mut merged: Vec<ExtentRef> = Vec::with_capacity(self.free_extent_addrs.len());
        for free in self.free_extent_addrs.drain(..) {
            if let Some(last) = merged.last_mut() {
                let last_end = last.addr.0 + last.len_bytes;
                if last_end == free.addr.0 {
                    last.len_bytes += free.len_bytes;
                    continue;
                }
            }
            merged.push(free);
        }
        self.free_extent_addrs = merged;
    }

    fn allocate_bucket_id(&mut self) -> BucketId {
        let head = self.free_buckets.head;
        if head.is_null() {
            return self.bucket_table.next_id();
        }

        let next = self
            .bucket_table
            .get(head)
            .map(|header| header.next)
            .unwrap_or(BucketId::NULL);
        self.free_buckets.head = next;
        if let Some(header) = self.bucket_table.get_mut(head) {
            header.next = BucketId::NULL;
        }
        head
    }

    fn allocate_bucket_addr(&mut self) -> StableAddr {
        let addr = self.next_extent_addr;
        self.next_extent_addr = StableAddr(addr.0 + self.bucket_size_bytes());
        addr
    }

    fn count_bucket_chain(&self, chain: BucketChain) -> usize {
        if chain.head.is_null() {
            return 0;
        }
        let mut count = 0usize;
        let mut cursor = chain.head;
        while !cursor.is_null() {
            count += 1;
            cursor = self
                .bucket_table
                .get(cursor)
                .map(|header| header.next)
                .unwrap_or(BucketId::NULL);
        }
        count
    }

    fn release_extent_id_internal(&mut self, id: ExtentId) {
        if let Some(header) = self.extent_table.get_mut(id) {
            header.next = self.free_extents.head;
            self.free_extents.head = id;
        }
    }

    #[cfg(test)]
    fn release_extent_id(&mut self, id: ExtentId) {
        self.release_extent_id_internal(id);
    }

    #[cfg(test)]
    fn release_bucket_id(&mut self, id: BucketId) {
        if let Some(header) = self.bucket_table.get_mut(id) {
            header.next = self.free_buckets.head;
            self.free_buckets.head = id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RegionManager;
    use crate::low_level::{
        BucketChain, BucketId, BucketSizeInPages, EdgeRef, EdgeSegmentHeader, EdgeSegmentState,
        ExtentChain, ExtentGrowthKind, ExtentGrowthPolicy, ExtentGrowthRequest, ExtentId,
        ExtentRef, RegionKind, StableAddr, WasmPages,
    };

    #[test]
    fn region_manager_tracks_extent_and_bucket_roots() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        let extent_region = manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(8),
                WasmPages::new(2),
            ),
        );
        let bucket_region = manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            BucketChain::new(BucketId::new(1), BucketId::new(1), 2048),
        );

        assert_eq!(extent_region.root, 1);
        assert_eq!(bucket_region.root, 1);
        assert!(
            manager
                .extent_chain(RegionKind::ForwardEdgeEntries)
                .is_some()
        );
        assert!(
            manager
                .bucket_chain(RegionKind::NodePropertyStore)
                .is_some()
        );
        assert_eq!(manager.extent_table.len(), 1);
        assert_eq!(manager.bucket_table.len(), 1);
        assert_eq!(
            manager
                .extent_table
                .get(ExtentId::new(1))
                .map(|header| header.extent.len_bytes),
            Some(8 * WasmPages::new(1).bytes())
        );
    }

    #[test]
    fn region_manager_resolves_segment_zero_for_edge_region() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                128,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let edge_ref = EdgeRef::new(0, 5);
        let (segment, extent) = manager
            .resolve_edge_ref(RegionKind::ForwardEdgeEntries, edge_ref)
            .expect("segment zero should resolve");

        assert_eq!(segment.segment_id, 0);
        assert_eq!(segment.state, EdgeSegmentState::Active);
        assert_eq!(segment.slot_capacity, 16);
        assert_eq!(extent.len_bytes / 8, 8192);
    }

    #[test]
    fn region_manager_registers_and_reads_explicit_edge_segments() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                64,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager
            .extent_table
            .insert(crate::low_level::ExtentHeader::new(
                ExtentId::new(9),
                ExtentRef::new(StableAddr(999), 320),
                ExtentId::NULL,
            ));
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(7, ExtentId::new(9), 40, 0, EdgeSegmentState::Active),
            )
            .expect("edge segment registration should succeed");

        let edge_ref = EdgeRef::new(7, 12);
        let (segment, extent) = manager
            .resolve_edge_ref(RegionKind::ForwardEdgeEntries, edge_ref)
            .expect("registered segment should resolve");

        assert_eq!(segment.segment_id, 7);
        assert_eq!(segment.slot_capacity, 40);
        assert_eq!(extent.addr, StableAddr(999));
        assert_eq!(
            manager.edge_ref_slot_capacity(RegionKind::ForwardEdgeEntries, edge_ref),
            Some(40)
        );
    }

    #[test]
    fn region_manager_tracks_edge_segment_lifecycle_states() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                64,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager
            .extent_table
            .insert(crate::low_level::ExtentHeader::new(
                ExtentId::new(9),
                ExtentRef::new(StableAddr(999), 320),
                ExtentId::NULL,
            ));
        let segment_id = manager
            .next_edge_segment_id(RegionKind::ForwardEdgeEntries)
            .expect("segment directory should exist");
        assert_eq!(segment_id, 1);
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(
                    segment_id,
                    ExtentId::new(9),
                    40,
                    0,
                    EdgeSegmentState::Active,
                ),
            )
            .expect("edge segment registration should succeed");

        let previous = manager
            .retire_edge_segment(RegionKind::ForwardEdgeEntries, segment_id, 42)
            .expect("retire should succeed");
        assert_eq!(previous.state, EdgeSegmentState::Active);
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                .unwrap()
                .state,
            EdgeSegmentState::Retired
        );
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                .unwrap()
                .retired_epoch,
            42
        );

        let previous = manager
            .reactivate_edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
            .expect("reactivate should succeed");
        assert_eq!(previous.state, EdgeSegmentState::Retired);
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                .unwrap()
                .state,
            EdgeSegmentState::Active
        );
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                .unwrap()
                .retired_epoch,
            0
        );

        let previous = manager
            .free_edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
            .expect("free should succeed");
        assert_eq!(previous.state, EdgeSegmentState::Active);
        assert_eq!(
            manager
                .edge_segment(RegionKind::ForwardEdgeEntries, segment_id)
                .unwrap()
                .state,
            EdgeSegmentState::Free
        );
    }

    #[test]
    fn region_manager_does_not_mutate_segment_zero_state() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                64,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        assert_eq!(
            manager.retire_edge_segment(RegionKind::ForwardEdgeEntries, 0, 1),
            None
        );
        assert_eq!(
            manager.reactivate_edge_segment(RegionKind::ForwardEdgeEntries, 0),
            None
        );
        assert_eq!(
            manager.free_edge_segment(RegionKind::ForwardEdgeEntries, 0),
            None
        );
    }

    #[test]
    fn region_manager_can_allocate_explicit_edge_segment_storage() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                64,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let segment = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 12, EdgeSegmentState::Active)
            .expect("segment allocation should succeed");
        let extent = manager
            .resolve_edge_ref(
                RegionKind::ForwardEdgeEntries,
                EdgeRef::from_raw((segment.segment_id as u64) << EdgeRef::START_SLOT_BITS),
            )
            .expect("allocated segment should resolve")
            .1;

        assert_eq!(segment.segment_id, 1);
        assert_eq!(segment.slot_capacity, 12);
        assert_eq!(extent.len_bytes, 96);
    }

    #[test]
    fn region_manager_can_replace_and_reclaim_edge_segment_storage() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                64,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let original = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 10, EdgeSegmentState::Active)
            .expect("segment allocation should succeed");

        let (replacement, retired) = manager
            .replace_edge_segment_for_maintenance(
                RegionKind::ForwardEdgeEntries,
                original.segment_id,
                14,
                77,
            )
            .expect("segment replacement should succeed");

        assert_eq!(replacement.segment_id, 2);
        assert_eq!(replacement.slot_capacity, 14);
        let retired = retired.expect("original segment should be retired");
        assert_eq!(retired.segment_id, original.segment_id);
        assert_eq!(retired.state, EdgeSegmentState::Retired);
        assert_eq!(retired.retired_epoch, 77);

        let freed = manager
            .free_edge_segment(RegionKind::ForwardEdgeEntries, original.segment_id)
            .expect("free transition should succeed");
        assert_eq!(freed.state, EdgeSegmentState::Retired);
        let reclaimed = manager
            .reclaim_edge_segment_storage(RegionKind::ForwardEdgeEntries, original.segment_id)
            .expect("reclaim should succeed");
        assert_eq!(reclaimed.segment_id, original.segment_id);

        let reused = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 10, EdgeSegmentState::Active)
            .expect("re-allocation should succeed");
        let reused_extent = manager
            .resolve_edge_ref(
                RegionKind::ForwardEdgeEntries,
                EdgeRef::from_raw((reused.segment_id as u64) << EdgeRef::START_SLOT_BITS),
            )
            .expect("reused segment should resolve")
            .1;
        assert_eq!(reused_extent.len_bytes, 80);
    }

    #[test]
    fn region_manager_can_sweep_retired_edge_segments() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                64,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let active = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 10, EdgeSegmentState::Active)
            .expect("segment allocation should succeed");
        manager
            .retire_edge_segment(RegionKind::ForwardEdgeEntries, active.segment_id, 50)
            .expect("retire should succeed");

        let swept = manager
            .sweep_retired_edge_segments(RegionKind::ForwardEdgeEntries, 60, 5)
            .expect("sweep should succeed");

        assert_eq!(swept.len(), 1);
        assert_eq!(swept[0].segment_id, active.segment_id);
        assert_eq!(swept[0].state, EdgeSegmentState::Free);
    }

    #[test]
    fn region_manager_reuses_free_allocator_ids() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager
            .extent_table
            .insert(crate::low_level::ExtentHeader::new(
                ExtentId::new(9),
                crate::low_level::ExtentRef::new(crate::low_level::StableAddr(0), 0),
                ExtentId::NULL,
            ));
        manager
            .bucket_table
            .insert(crate::low_level::BucketHeader::new(
                BucketId::new(7),
                crate::low_level::StableAddr(0),
                BucketId::NULL,
            ));
        manager.release_extent_id(ExtentId::new(9));
        manager.release_bucket_id(BucketId::new(7));

        let extent_region = manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                4096,
                WasmPages::new(8),
                WasmPages::new(2),
            ),
        );
        let bucket_region = manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 2048),
        );

        assert_eq!(extent_region.root, 1);
        assert_eq!(bucket_region.root, 1);
        assert!(manager.extent_table.get(ExtentId::new(9)).is_some());
        assert!(manager.bucket_table.get(BucketId::new(7)).is_some());
    }

    #[test]
    fn region_manager_uses_chained_free_ids_in_lifo_order() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager
            .extent_table
            .insert(crate::low_level::ExtentHeader::new(
                ExtentId::new(9),
                crate::low_level::ExtentRef::new(crate::low_level::StableAddr(0), 0),
                ExtentId::NULL,
            ));
        manager
            .extent_table
            .insert(crate::low_level::ExtentHeader::new(
                ExtentId::new(10),
                crate::low_level::ExtentRef::new(crate::low_level::StableAddr(0), 0),
                ExtentId::NULL,
            ));
        manager.release_extent_id(ExtentId::new(9));
        manager.release_extent_id(ExtentId::new(10));

        let first = manager.allocate_extent_id();
        let second = manager.allocate_extent_id();

        assert_eq!(first, ExtentId::new(10));
        assert_eq!(second, ExtentId::new(9));
        assert!(manager.free_extents.is_empty());
    }

    #[test]
    fn region_manager_plans_and_applies_extent_growth() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(8),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(3),
                WasmPages::new(1),
            ),
        );

        let request = ExtentGrowthRequest::new(WasmPages::new(4));
        let policy = ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8));
        let decision = manager
            .plan_extent_growth(RegionKind::ForwardEdgeEntries, request, policy)
            .expect("extent region should exist");
        let updated = manager
            .apply_extent_growth(RegionKind::ForwardEdgeEntries, request, policy, decision)
            .expect("extent region should update");

        assert_eq!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        assert_eq!(updated.allocated_pages.raw, 11);
        assert_eq!(updated.slack_pages.raw, 0);
        assert_eq!(
            manager
                .extent_table
                .get(ExtentId::new(1))
                .map(|header| header.extent.len_bytes),
            Some(WasmPages::new(11).bytes())
        );
    }

    #[test]
    fn region_manager_allocates_extent_addresses_monotonically() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(2),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(3),
                WasmPages::new(1),
            ),
        );

        let first = manager
            .extent_table
            .get(ExtentId::new(1))
            .expect("first extent");
        let second = manager
            .extent_table
            .get(ExtentId::new(2))
            .expect("second extent");

        assert_eq!(first.extent.addr.0, 0);
        assert_eq!(first.extent.len_bytes, WasmPages::new(2).bytes());
        assert_eq!(second.extent.addr.0, WasmPages::new(2).bytes());
        assert_eq!(second.extent.len_bytes, WasmPages::new(3).bytes());
        assert_eq!(manager.next_extent_addr.0, WasmPages::new(5).bytes());
    }

    #[test]
    fn relocate_self_reassigns_extent_address() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );
        let before = manager
            .extent_table
            .get(ExtentId::new(1))
            .expect("extent should exist")
            .extent
            .addr;

        let request = ExtentGrowthRequest::new(WasmPages::new(4));
        let policy = ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(1));
        let decision = manager
            .plan_extent_growth(RegionKind::ForwardEdgeEntries, request, policy)
            .expect("extent region should exist");
        let updated = manager
            .apply_extent_growth(RegionKind::ForwardEdgeEntries, request, policy, decision)
            .expect("extent region should update");
        let after = manager
            .extent_table
            .get(ExtentId::new(1))
            .expect("extent should exist")
            .extent
            .addr;

        assert_eq!(decision.growth_kind(), ExtentGrowthKind::RelocateSelf);
        assert_ne!(before, after);
        assert_eq!(updated.allocated_pages.raw, 6);
    }

    #[test]
    fn relocating_trailing_small_region_reassigns_trailing_address() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(8),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(3),
                WasmPages::new(1),
            ),
        );

        let trailing_before = manager
            .extent_table
            .get(ExtentId::new(2))
            .expect("trailing extent should exist")
            .extent
            .addr;

        let request = ExtentGrowthRequest::new(WasmPages::new(4));
        let policy = ExtentGrowthPolicy::new(WasmPages::new(2), WasmPages::new(8));
        let decision = manager
            .plan_extent_growth(RegionKind::ForwardEdgeEntries, request, policy)
            .expect("extent region should exist");

        manager
            .apply_extent_growth(RegionKind::ForwardEdgeEntries, request, policy, decision)
            .expect("extent region should update");

        let trailing_after = manager
            .extent_table
            .get(ExtentId::new(2))
            .expect("trailing extent should exist")
            .extent
            .addr;

        assert_eq!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        assert_ne!(trailing_before, trailing_after);
    }

    #[test]
    fn region_manager_finds_immediate_trailing_extent_region_by_address() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );
        manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            BucketChain::new(BucketId::new(1), BucketId::new(1), 2048),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(3),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::new(3),
                ExtentId::new(3),
                4096,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        assert_eq!(
            manager.next_extent_region_after(RegionKind::ForwardEdgeEntries),
            Some(RegionKind::ReverseEdgeEntries)
        );
        assert_eq!(
            manager.next_extent_region_after(RegionKind::ReverseEdgeEntries),
            Some(RegionKind::ForwardSegmentLog)
        );
    }

    #[test]
    fn region_manager_collects_all_colliding_trailing_extent_regions() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(4),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::new(3),
                ExtentId::new(3),
                4096,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseSegmentLog,
            ExtentChain::new(
                ExtentId::new(4),
                ExtentId::new(4),
                4096,
                WasmPages::new(5),
                WasmPages::new(0),
            ),
        );

        let colliding = manager
            .colliding_trailing_extent_regions(RegionKind::ForwardEdgeEntries, WasmPages::new(3));
        assert_eq!(
            colliding,
            vec![
                RegionKind::ReverseEdgeEntries,
                RegionKind::ForwardSegmentLog
            ]
        );
        assert_eq!(
            manager.colliding_trailing_extent_region_pages(
                RegionKind::ForwardEdgeEntries,
                WasmPages::new(3)
            ),
            WasmPages::new(3)
        );
    }

    #[test]
    fn relocating_small_trailing_regions_reassigns_all_colliding_addresses() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(4),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::new(3),
                ExtentId::new(3),
                4096,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let reverse_before = manager
            .extent_table
            .get(ExtentId::new(2))
            .expect("reverse extent should exist")
            .extent
            .addr;
        let log_before = manager
            .extent_table
            .get(ExtentId::new(3))
            .expect("log extent should exist")
            .extent
            .addr;

        let request = ExtentGrowthRequest::new(WasmPages::new(3));
        let policy = ExtentGrowthPolicy::new(WasmPages::new(1), WasmPages::new(4));
        let decision = manager
            .plan_extent_growth(RegionKind::ForwardEdgeEntries, request, policy)
            .expect("extent region should exist");

        manager
            .apply_extent_growth(RegionKind::ForwardEdgeEntries, request, policy, decision)
            .expect("extent region should update");

        let reverse_after = manager
            .extent_table
            .get(ExtentId::new(2))
            .expect("reverse extent should exist")
            .extent
            .addr;
        let log_after = manager
            .extent_table
            .get(ExtentId::new(3))
            .expect("log extent should exist")
            .extent
            .addr;

        assert_eq!(
            decision.growth_kind(),
            ExtentGrowthKind::RelocateTrailingSmallRegions
        );
        assert_ne!(reverse_before, reverse_after);
        assert_ne!(log_before, log_after);
    }

    #[test]
    fn relocate_self_reuses_freed_extent_addresses_before_bumping() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::new(1),
                ExtentId::new(1),
                4096,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::new(2),
                ExtentId::new(2),
                4096,
                WasmPages::new(2),
                WasmPages::new(0),
            ),
        );

        let forward_before = manager
            .extent_table
            .get(ExtentId::new(1))
            .expect("forward extent should exist")
            .extent;

        let request = ExtentGrowthRequest::new(WasmPages::new(3));
        let policy = ExtentGrowthPolicy::new(WasmPages::new(1), WasmPages::new(1));
        let decision = manager
            .plan_extent_growth(RegionKind::ForwardEdgeEntries, request, policy)
            .expect("extent region should exist");
        manager
            .apply_extent_growth(RegionKind::ForwardEdgeEntries, request, policy, decision)
            .expect("extent region should update");

        let new_region = manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::new(3),
                ExtentId::new(3),
                4096,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let new_extent = manager
            .extent_chain(new_region.region_kind())
            .and_then(|chain| manager.extent_table.get(chain.head).copied())
            .expect("new extent should exist")
            .extent;

        assert_eq!(decision.growth_kind(), ExtentGrowthKind::RelocateSelf);
        assert_eq!(new_extent.addr, forward_before.addr);
    }

    #[test]
    fn freed_extent_spans_are_merged_for_reuse() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.free_extent_addr(ExtentRef::new(StableAddr(0), WasmPages::new(2).bytes()));
        manager.free_extent_addr(ExtentRef::new(
            StableAddr(WasmPages::new(2).bytes()),
            WasmPages::new(3).bytes(),
        ));

        let addr = manager.allocate_extent_addr(WasmPages::new(5).bytes());
        assert_eq!(addr, StableAddr(0));
        assert!(manager.free_extent_addrs.is_empty());
    }
}
