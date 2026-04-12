//! Property equality index: in-memory [`PropertyIndex`] views plus **PIDX v3** persistence via a
//! single [`PropertyEqualityStableMap`] (`ic_stable_structures::StableBTreeMap`) behind
//! [`encode_pidx_v3_region`] / [`decode_pidx_v3_region`].
//!
//! Byte-ordered keys ([`PropertyIndexKey`]), optional [`PropertyIndexSnapshot`] encoding,
//! and stable-memory region I/O live here.

mod errors;
mod pidx_v3_layout;
mod types;

pub use errors::{PropertyIndexError, PropertyIndexLeafChainShapeError};
pub use pidx_v3_layout::{
    PIDX_V3_HEADER_LEN, PIDX_V3_LAYOUT_VERSION, PIDX_V3_MAGIC, PropertyIndexRegionHeaderV3,
};
pub use types::{
    PropertyIndex, PropertyIndexAllocatorHeader, PropertyIndexEntityKind, PropertyIndexEntry,
    PropertyIndexHeader, PropertyIndexKey, PropertyIndexNodeHeader, PropertyIndexNodeId,
    PropertyIndexNodeKind, PropertyIndexNodeRecord, PropertyIndexSnapshot,
};

mod ic_pidx_linear_memory;
mod mutation_telemetry;
mod property_equality;

pub use ic_pidx_linear_memory::PropertyIndexBtreeSubregionIcMemory;
pub use mutation_telemetry::{PropertyIndexNodeStoreDelta, PropertyIndexNodeStoreMutationKind};
pub use property_equality::{
    FixedSlotPropertyEqualityMap, PropertyEqualityInplaceMap, PropertyEqualityStableMap,
    build_equality_map_from_snapshot,
    clone_property_equality_map, decode_pidx_v3_region, empty_property_equality_inplace_map,
    empty_property_equality_map, encode_pidx_v3_region, hydrate_property_equality_inplace_map,
    hydrate_property_equality_map_from_serialized_bytes, serialize_property_equality_btree,
    open_fixed_slot_property_equality_map, replace_fixed_slot_property_equality_map,
    serialize_property_equality_map, snapshot_fixed_slot_property_equality_map,
    snapshot_from_equality_any_memory, snapshot_from_equality_map,
};

/// PIDX v3 tooling image: logical snapshot + one persisted equality btree (see `encode` / `decode`).
pub struct PropertyIndexStorageImage {
    pub snapshot: PropertyIndexSnapshot,
    pub equality_map: PropertyEqualityStableMap,
}

impl Clone for PropertyIndexStorageImage {
    fn clone(&self) -> Self {
        Self {
            snapshot: self.snapshot.clone(),
            equality_map: clone_property_equality_map(&self.equality_map),
        }
    }
}

impl PartialEq for PropertyIndexStorageImage {
    fn eq(&self, other: &Self) -> bool {
        self.snapshot == other.snapshot
            && serialize_property_equality_map(&self.equality_map)
                == serialize_property_equality_map(&other.equality_map)
    }
}

impl Eq for PropertyIndexStorageImage {}

impl std::fmt::Debug for PropertyIndexStorageImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PropertyIndexStorageImage")
            .field("snapshot", &self.snapshot)
            .field("equality_entries", &self.equality_map.len())
            .finish()
    }
}

const STORAGE_IMAGE_BRANCHING: u16 = 64;

impl PropertyIndexStorageImage {
    pub fn try_from_indices(
        snapshot: PropertyIndexSnapshot,
        _page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        Ok(Self {
            equality_map: build_equality_map_from_snapshot(&snapshot),
            snapshot,
        })
    }

    pub fn try_from_sectioned_parts(
        snapshot: PropertyIndexSnapshot,
        equality_map: PropertyEqualityStableMap,
        branching_factor: u16,
        page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        let mut s = Self {
            snapshot,
            equality_map,
        };
        s.try_reconcile(branching_factor, page_size_bytes)?;
        Ok(s)
    }

    pub fn empty(branching_factor: u16, page_size_bytes: u32) -> Self {
        Self::try_from_indices(
            PropertyIndexSnapshot::empty(branching_factor),
            page_size_bytes,
        )
        .expect("empty")
    }

    pub fn rebuild_snapshot_from_equality_map(&mut self, branching_factor: u16) {
        self.snapshot = snapshot_from_equality_map(&self.equality_map, branching_factor);
    }

    pub fn try_normalized(
        mut self,
        branching_factor: u16,
        page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        self.try_reconcile(branching_factor, page_size_bytes)?;
        Ok(self)
    }

    pub fn try_reconcile(
        &mut self,
        branching_factor: u16,
        _page_size_bytes: u32,
    ) -> Result<(), PropertyIndexError> {
        self.snapshot = snapshot_from_equality_map(&self.equality_map, branching_factor);
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        encode_pidx_v3_region(&self.equality_map)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        let equality_map = decode_pidx_v3_region(bytes)?;
        let snapshot = snapshot_from_equality_map(&equality_map, STORAGE_IMAGE_BRANCHING);
        Ok(Self {
            snapshot,
            equality_map,
        })
    }
}

mod storage;

pub use storage::{
    ensure_pidx_v3_btree_subregion_for_hydrate, read_pidx_v3_header_from_stable_memory,
    read_property_index_region_bytes, read_property_index_region_header_from_stable_memory,
    read_property_index_region_magic, read_property_index_snapshot_from_stable_memory,
    read_property_index_snapshot_section_from_stable_memory,
    read_property_index_storage_image_from_stable_memory,
    scan_edge_property_index_property_prefix_from_stable_memory,
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
    sync_property_index_pidx_v3_header_to_stable_memory,
    write_property_index_snapshot_to_stable_memory,
    write_property_index_stable_equality_to_stable_memory,
    write_property_index_storage_image_to_stable_memory,
};

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::NodeId;

    #[test]
    fn v3_storage_image_round_trips() {
        let mut snap = PropertyIndexSnapshot::empty(64);
        snap.node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "p", vec![1]),
            PropertyIndexEntry::empty(),
        );
        let img = PropertyIndexStorageImage::try_from_indices(snap, 4096).expect("build");
        let bytes = img.encode().expect("enc");
        let dec = PropertyIndexStorageImage::decode(&bytes).expect("dec");
        assert_eq!(
            dec.snapshot.node_index.header.entry_count,
            img.snapshot.node_index.header.entry_count
        );
    }
}
