//! Stable-memory region directory and page-granularity primitives.

use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Stable-memory page size used by the allocator layer.
pub const WASM_PAGE_SIZE: u64 = 65_536;
/// Maximum number of logical region kinds tracked by the directory.
pub const MAX_REGION_KINDS: usize = 32;

/// One WebAssembly stable-memory page.
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
pub struct WasmPages {
    pub raw: u64,
}

impl WasmPages {
    /// Creates a page-count wrapper.
    pub const fn new(raw: u64) -> Self {
        Self { raw }
    }

    /// Converts this page count into bytes.
    pub const fn bytes(self) -> u64 {
        self.raw * WASM_PAGE_SIZE
    }
}

/// Bucket size chosen for bucket-chain-backed regions.
///
/// This belongs to the allocator layer, not the adjacency-kernel layer.
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
pub struct BucketSizeInPages {
    pub raw: u16,
}

impl BucketSizeInPages {
    /// Default bucket size in wasm pages.
    pub const DEFAULT: Self = Self { raw: 128 };

    /// Creates an explicit bucket size in wasm pages.
    pub const fn new(raw: u16) -> Self {
        Self { raw }
    }

    /// Converts this bucket size into bytes.
    pub const fn bytes(self) -> u64 {
        self.raw as u64 * WASM_PAGE_SIZE
    }
}

/// Physical backing strategy used by a region.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, CandidType)]
pub enum RegionStorageKind {
    Extent = 0,
    BucketChain = 1,
}

/// Logical tenant inside the stable-memory region manager.
///
/// Each kind names one well-known storage role such as forward edge entries,
/// reverse segment logs, or the property index.
#[repr(u16)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub enum RegionKind {
    ForwardVertexTable = 0,
    ForwardEdgeEntries = 1,
    ForwardLabelIndex = 2,
    ForwardSegmentLog = 3,
    ReverseVertexTable = 4,
    ReverseEdgeEntries = 5,
    ReverseLabelIndex = 6,
    ReverseSegmentLog = 7,
    NodePropertyStore = 8,
    EdgePropertyStore = 9,
    PropertyIndex = 10,
    LabelCatalog = 11,
    GcState = 12,
    MaintenanceQueue = 13,
}

impl RegionKind {
    /// Returns the fixed directory slot reserved for this region kind.
    pub const fn slot(self) -> usize {
        self as usize
    }
}

/// Directory record for one region.
///
/// `root` is interpreted according to [`RegionStorageKind`]:
/// - `Extent`: root `ExtentChain` slot
/// - `BucketChain`: first bucket-chain root slot
///
/// Invariant:
/// - `kind` names the logical tenant
/// - `storage` names how that tenant is physically backed
/// - `logical_len_bytes` is the logical payload length, not allocator slack
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct RegionRef {
    pub storage: u8,
    pub reserved: u8,
    pub kind: u16,
    pub root: u32,
    pub logical_len_bytes: u64,
}

impl RegionRef {
    /// Creates one logical region reference recorded in the directory.
    pub const fn new(
        storage: RegionStorageKind,
        kind: RegionKind,
        root: u32,
        logical_len_bytes: u64,
    ) -> Self {
        Self {
            storage: storage as u8,
            reserved: 0,
            kind: kind as u16,
            root,
            logical_len_bytes,
        }
    }

    /// Decodes the stored storage tag into a typed storage kind.
    pub fn storage_kind(self) -> RegionStorageKind {
        match self.storage {
            0 => RegionStorageKind::Extent,
            1 => RegionStorageKind::BucketChain,
            _ => panic!("invalid region storage kind"),
        }
    }

    /// Decodes the stored region tag into a typed region kind.
    pub fn region_kind(self) -> RegionKind {
        match self.kind {
            0 => RegionKind::ForwardVertexTable,
            1 => RegionKind::ForwardEdgeEntries,
            2 => RegionKind::ForwardLabelIndex,
            3 => RegionKind::ForwardSegmentLog,
            4 => RegionKind::ReverseVertexTable,
            5 => RegionKind::ReverseEdgeEntries,
            6 => RegionKind::ReverseLabelIndex,
            7 => RegionKind::ReverseSegmentLog,
            8 => RegionKind::NodePropertyStore,
            9 => RegionKind::EdgePropertyStore,
            10 => RegionKind::PropertyIndex,
            11 => RegionKind::LabelCatalog,
            12 => RegionKind::GcState,
            13 => RegionKind::MaintenanceQueue,
            _ => panic!("invalid region kind"),
        }
    }
}

/// One entry in the stable-memory region directory.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize, CandidType)]
pub struct RegionDirectoryEntry {
    pub region: RegionRef,
}

impl RegionDirectoryEntry {
    /// Wraps one logical region reference as a directory entry.
    pub const fn new(region: RegionRef) -> Self {
        Self { region }
    }
}

/// Fixed directory of currently defined regions.
///
/// This is metadata only; it does not store region payload bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RegionDirectory {
    entries: [Option<RegionDirectoryEntry>; MAX_REGION_KINDS],
}

impl Default for RegionDirectory {
    fn default() -> Self {
        Self {
            entries: [None; MAX_REGION_KINDS],
        }
    }
}

impl RegionDirectory {
    /// Looks up one region definition by logical kind.
    pub fn get(&self, kind: RegionKind) -> Option<RegionRef> {
        self.entries[kind.slot()].map(|entry| entry.region)
    }

    /// Inserts or replaces the directory entry for `region.kind`.
    pub fn set(&mut self, region: RegionRef) {
        let kind = region.region_kind();
        self.entries[kind.slot()] = Some(RegionDirectoryEntry::new(region));
    }

    /// Removes the directory entry for the given region kind.
    pub fn clear(&mut self, kind: RegionKind) {
        self.entries[kind.slot()] = None;
    }

    /// Iterates over all currently defined directory entries.
    pub fn iter(&self) -> impl Iterator<Item = RegionDirectoryEntry> + '_ {
        self.entries.iter().flatten().copied()
    }
}

/// Top-level metadata layout for the region manager.
///
/// This records which regions exist and the allocator granularity used for
/// bucket-backed tenants.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct RegionManagerLayout {
    pub directory: RegionDirectory,
    pub bucket_size_in_pages: BucketSizeInPages,
}

impl RegionManagerLayout {
    /// Creates an empty region-manager layout with a chosen bucket granularity.
    pub fn with_bucket_size(bucket_size_in_pages: BucketSizeInPages) -> Self {
        Self {
            directory: RegionDirectory::default(),
            bucket_size_in_pages,
        }
    }

    /// Records one region definition in the directory.
    pub fn define_region(&mut self, region: RegionRef) {
        self.directory.set(region);
    }

    /// Returns the directory entry for the requested region kind.
    pub fn region(&self, kind: RegionKind) -> Option<RegionRef> {
        self.directory.get(kind)
    }

    /// Returns whether a region of the given kind is defined.
    pub fn has_region(&self, kind: RegionKind) -> bool {
        self.region(kind).is_some()
    }
}

const _: [(); 16] = [(); core::mem::size_of::<RegionRef>()];
const _: [(); 16] = [(); core::mem::size_of::<RegionDirectoryEntry>()];
const _: [(); 8] = [(); core::mem::size_of::<WasmPages>()];
const _: [(); 2] = [(); core::mem::size_of::<BucketSizeInPages>()];

#[cfg(test)]
mod tests {
    use super::{
        BucketSizeInPages, RegionDirectory, RegionKind, RegionManagerLayout, RegionRef,
        RegionStorageKind, WASM_PAGE_SIZE, WasmPages,
    };

    #[test]
    fn region_refs_support_mixed_storage_kinds() {
        let forward = RegionRef::new(
            RegionStorageKind::Extent,
            RegionKind::ForwardEdgeEntries,
            11,
            8192,
        );
        let props = RegionRef::new(
            RegionStorageKind::BucketChain,
            RegionKind::NodePropertyStore,
            7,
            2048,
        );

        assert_eq!(forward.storage_kind(), RegionStorageKind::Extent);
        assert_eq!(forward.region_kind(), RegionKind::ForwardEdgeEntries);
        assert_eq!(props.storage_kind(), RegionStorageKind::BucketChain);
        assert_eq!(props.region_kind(), RegionKind::NodePropertyStore);
    }

    #[test]
    fn region_directory_tracks_regions_by_kind() {
        let mut directory = RegionDirectory::default();
        let region = RegionRef::new(
            RegionStorageKind::BucketChain,
            RegionKind::NodePropertyStore,
            21,
            4096,
        );
        directory.set(region);

        assert_eq!(directory.get(RegionKind::NodePropertyStore), Some(region));
        assert_eq!(directory.get(RegionKind::EdgePropertyStore), None);
    }

    #[test]
    fn region_manager_layout_defines_regions() {
        let mut layout = RegionManagerLayout::with_bucket_size(BucketSizeInPages::DEFAULT);
        layout.define_region(RegionRef::new(
            RegionStorageKind::Extent,
            RegionKind::ForwardEdgeEntries,
            2,
            8192,
        ));

        assert!(layout.has_region(RegionKind::ForwardEdgeEntries));
        assert!(!layout.has_region(RegionKind::ReverseEdgeEntries));
        assert_eq!(layout.bucket_size_in_pages.bytes(), 128 * WASM_PAGE_SIZE);
    }

    #[test]
    fn wasm_page_units_convert_to_bytes() {
        assert_eq!(WasmPages::new(2).bytes(), 2 * WASM_PAGE_SIZE);
        assert_eq!(BucketSizeInPages::new(4).bytes(), 4 * WASM_PAGE_SIZE);
    }
}
