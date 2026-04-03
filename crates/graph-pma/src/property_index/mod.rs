//! Rewrite-side property-index skeleton.
//!
//! This module defines the low-level boundaries for the future bucket-backed
//! property index. The intended semantics are closer to a leaf-linked,
//! high-fanout `(a,b)`-tree than to the old whole-tree snapshot backend.
//!
//! The current implementation intentionally stops short of the full tree.
//! It fixes the byte encodings and in-memory entry model first:
//!
//! - index header metadata
//! - byte-ordered equality keys
//! - leaf-entry payload boundary
//! - simple ordered in-memory entry set for exact and prefix scans
//!
//! Incremental mutations that **persist** updated leaf records require each leaf
//! record to fit in one node page (same rule as [`PropertyIndexNodeStore::encode_node_page`]).
//! Helpers may still use multi-page encoding internally for experiments or I/O plumbing.
//!
//! Treat [`PropertyIndexNodeStore::encode_node_page`] as the gate for persisted leaves: incremental
//! redistribution, merge, and three-leaf repack algorithms only commit updates that pass that
//! encoding for the configured page size.

mod errors;
mod types;

pub use errors::{PropertyIndexError, PropertyIndexLeafChainShapeError};
pub use types::{
    PropertyIndex, PropertyIndexAllocatorHeader, PropertyIndexEntityKind, PropertyIndexEntry,
    PropertyIndexHeader, PropertyIndexKey, PropertyIndexNodeHeader, PropertyIndexNodeId,
    PropertyIndexNodeKind, PropertyIndexNodeRecord, PropertyIndexSnapshot,
};

mod node_store;

pub use node_store::{
    PropertyIndexNodeStore, PropertyIndexNodeStoreDelta, PropertyIndexNodeStoreMutationKind,
};

/// Stable-memory image: PIDX logical snapshot plus paged node-store areas for node/edge indices.
///
/// **Invariant (compact writeback / hydration):** facades may persist an **empty**
/// [`PropertyIndexSnapshot`] while [`PropertyIndexNodeStore`] pages still hold the authoritative
/// tree. Loading bytes therefore **must not** conclude “no property index” from the snapshot
/// section alone. [`Self::try_normalized`] / [`Self::try_reconcile`] rebuild `snapshot` from non-empty
/// stores when the logical side reports zero entries; [`crate::RewriteGraphPma::hydrate_from_stable_memory`]
/// does this via [`Self::from_sectioned_parts`].
///
/// After normalization, in-memory logical indices match the node stores and scans behave as if
/// the snapshot had been fully populated on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyIndexStorageImage {
    pub snapshot: PropertyIndexSnapshot,
    pub node_store: PropertyIndexNodeStore,
    pub edge_store: PropertyIndexNodeStore,
}

/// Fixed-width region header for the property-index region payload.
///
/// This keeps the section boundaries explicit before the property index is
/// rewritten to read node pages directly from the region.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyIndexRegionHeader {
    pub version: u8,
    pub reserved: [u8; 3],
    pub snapshot_len: u32,
    pub node_store_len: u32,
    pub edge_store_len: u32,
}

impl PropertyIndexRegionHeader {
    /// Region payload magic.
    pub const MAGIC: [u8; 4] = *b"PIDN";

    /// Fixed encoded length.
    pub const ENCODED_LEN: usize = 4 + 1 + 3 + 4 + 4 + 4;

    /// Encodes one fixed-width region header.
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..4].copy_from_slice(&Self::MAGIC);
        out[4] = self.version;
        out[5..8].copy_from_slice(&self.reserved);
        out[8..12].copy_from_slice(&self.snapshot_len.to_le_bytes());
        out[12..16].copy_from_slice(&self.node_store_len.to_le_bytes());
        out[16..20].copy_from_slice(&self.edge_store_len.to_le_bytes());
        out
    }

    /// Decodes one fixed-width region header.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(PropertyIndexError::InvalidRegionHeaderLength(bytes.len()));
        }
        if bytes[..4] != Self::MAGIC {
            return Err(PropertyIndexError::InvalidMagic(bytes[..4].to_vec()));
        }
        let mut snapshot_len = [0u8; 4];
        snapshot_len.copy_from_slice(&bytes[8..12]);
        let mut node_store_len = [0u8; 4];
        node_store_len.copy_from_slice(&bytes[12..16]);
        let mut edge_store_len = [0u8; 4];
        edge_store_len.copy_from_slice(&bytes[16..20]);
        Ok(Self {
            version: bytes[4],
            reserved: [bytes[5], bytes[6], bytes[7]],
            snapshot_len: u32::from_le_bytes(snapshot_len),
            node_store_len: u32::from_le_bytes(node_store_len),
            edge_store_len: u32::from_le_bytes(edge_store_len),
        })
    }
}

impl PropertyIndexStorageImage {
    /// Current storage-image layout version.
    pub const VERSION: u8 = 2;

    /// Builds one storage image directly from logical indices.
    pub fn try_from_indices(
        snapshot: PropertyIndexSnapshot,
        page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        let node_store =
            PropertyIndexNodeStore::try_from_index(&snapshot.node_index, page_size_bytes)?;
        let edge_store =
            PropertyIndexNodeStore::try_from_index(&snapshot.edge_index, page_size_bytes)?;
        Ok(Self {
            snapshot,
            node_store,
            edge_store,
        })
    }

    /// Builds one storage image from already-decoded section payloads and
    /// normalizes it toward the node-store-primary persisted shape.
    pub fn try_from_sectioned_parts(
        snapshot: PropertyIndexSnapshot,
        node_store: PropertyIndexNodeStore,
        edge_store: PropertyIndexNodeStore,
        branching_factor: u16,
        page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        Self {
            snapshot,
            node_store,
            edge_store,
        }
        .try_normalized(branching_factor, page_size_bytes)
    }

    /// Creates one empty storage image with matching logical/node-store state.
    pub fn empty(branching_factor: u16, page_size_bytes: u32) -> Self {
        Self::try_from_indices(
            PropertyIndexSnapshot::empty(branching_factor),
            page_size_bytes,
        )
        .expect("empty snapshot yields valid empty node stores")
    }

    /// Rebuilds logical indices from persisted node stores when they are present.
    pub fn rebuild_snapshot_from_node_stores(&mut self, branching_factor: u16) {
        if !self.node_store.nodes.is_empty() {
            self.snapshot.node_index = self.node_store.to_index(branching_factor);
        }
        if !self.edge_store.nodes.is_empty() {
            self.snapshot.edge_index = self.edge_store.to_index(branching_factor);
        }
    }

    /// Returns one image normalized so persisted node stores can act as the
    /// primary representation while missing sides are still reconstructed.
    pub fn try_normalized(
        mut self,
        branching_factor: u16,
        page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        self.rebuild_snapshot_from_node_stores(branching_factor);
        self.try_reconcile(branching_factor, page_size_bytes)?;
        Ok(self)
    }

    /// Reconciles logical and persisted representations after decode or fallback.
    ///
    /// Preference order:
    /// - if the logical snapshot is **non-empty** (has entries / metadata), derive missing node
    ///   stores from it — this is the legacy and tooling-oriented path
    /// - if a logical side is empty but its node store has content, rebuild the
    ///   logical side from the node store (node-store-primary / compact-disk layout)
    ///
    /// Together with [`Self::rebuild_snapshot_from_node_stores`], this is what makes **hydration**
    /// correct when the on-disk PIDX section is empty but paged stores are not.
    pub fn try_reconcile(
        &mut self,
        branching_factor: u16,
        page_size_bytes: u32,
    ) -> Result<(), PropertyIndexError> {
        let node_snapshot_empty = self.snapshot.node_index.header.entry_count == 0;
        let edge_snapshot_empty = self.snapshot.edge_index.header.entry_count == 0;
        let node_store_empty = self.node_store.nodes.is_empty();
        let edge_store_empty = self.edge_store.nodes.is_empty();

        if node_snapshot_empty && !node_store_empty {
            self.snapshot.node_index = self.node_store.to_index(branching_factor);
        } else if !node_snapshot_empty && node_store_empty {
            self.node_store =
                PropertyIndexNodeStore::try_from_index(&self.snapshot.node_index, page_size_bytes)?;
        }

        if edge_snapshot_empty && !edge_store_empty {
            self.snapshot.edge_index = self.edge_store.to_index(branching_factor);
        } else if !edge_snapshot_empty && edge_store_empty {
            self.edge_store =
                PropertyIndexNodeStore::try_from_index(&self.snapshot.edge_index, page_size_bytes)?;
        }
        Ok(())
    }

    /// Encodes one full storage image to stable bytes.
    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let snapshot_bytes = self.snapshot.encode()?;
        let node_store_bytes = self.node_store.encode_paged_area()?;
        let edge_store_bytes = self.edge_store.encode_paged_area()?;
        let header = PropertyIndexRegionHeader {
            version: Self::VERSION,
            reserved: [0; 3],
            snapshot_len: u32::try_from(snapshot_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
            node_store_len: u32::try_from(node_store_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
            edge_store_len: u32::try_from(edge_store_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
        };
        let mut out = Vec::new();
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&snapshot_bytes);
        out.extend_from_slice(&node_store_bytes);
        out.extend_from_slice(&edge_store_bytes);
        Ok(out)
    }

    /// Encodes the compact flush shape (empty logical [`PropertyIndexSnapshot`] + paged stores)
    /// without cloning the node stores — same layout as [`Self::encode`] for the compact flush
    /// shape (empty logical snapshot + paged stores).
    pub fn encode_empty_snapshot_with_paged_stores(
        branching_factor: u16,
        node_store: &PropertyIndexNodeStore,
        edge_store: &PropertyIndexNodeStore,
    ) -> Result<Vec<u8>, PropertyIndexError> {
        let snapshot = PropertyIndexSnapshot::empty(branching_factor);
        let snapshot_bytes = snapshot.encode()?;
        let node_store_bytes = node_store.encode_paged_area()?;
        let edge_store_bytes = edge_store.encode_paged_area()?;
        let header = PropertyIndexRegionHeader {
            version: Self::VERSION,
            reserved: [0; 3],
            snapshot_len: u32::try_from(snapshot_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
            node_store_len: u32::try_from(node_store_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
            edge_store_len: u32::try_from(edge_store_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
        };
        let mut out = Vec::new();
        out.extend_from_slice(&header.encode());
        out.extend_from_slice(&snapshot_bytes);
        out.extend_from_slice(&node_store_bytes);
        out.extend_from_slice(&edge_store_bytes);
        Ok(out)
    }

    /// Decodes one full storage image from stable bytes.
    ///
    /// Payloads produced by [`Self::encode`] round-trip bit-for-bit. If bytes came from a **compact**
    /// writer that stores an empty [`PropertyIndexSnapshot`] alongside non-empty node stores, the
    /// decoded `snapshot` remains empty until you call [`Self::try_normalized`] (or [`Self::try_reconcile`])
    /// before using the logical indices.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() < PropertyIndexRegionHeader::ENCODED_LEN {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        let header =
            PropertyIndexRegionHeader::decode(&bytes[..PropertyIndexRegionHeader::ENCODED_LEN])?;
        let version = header.version;
        let snapshot_len = header.snapshot_len as usize;
        let node_store_len = header.node_store_len as usize;
        let edge_store_len = header.edge_store_len as usize;
        let snapshot_start = PropertyIndexRegionHeader::ENCODED_LEN;
        let snapshot_end = snapshot_start
            .checked_add(snapshot_len)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let node_store_end = snapshot_end
            .checked_add(node_store_len)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let edge_store_end = node_store_end
            .checked_add(edge_store_len)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        if edge_store_end != bytes.len() {
            return Err(PropertyIndexError::RecordLengthMismatch {
                expected: edge_store_end,
                actual: bytes.len(),
            });
        }
        let snapshot = PropertyIndexSnapshot::decode(&bytes[snapshot_start..snapshot_end])?;
        let (node_store, edge_store) = match version {
            1 => (
                PropertyIndexNodeStore::decode(&bytes[snapshot_end..node_store_end])?,
                PropertyIndexNodeStore::decode(&bytes[node_store_end..edge_store_end])?,
            ),
            2 => (
                PropertyIndexNodeStore::decode_paged_area(&bytes[snapshot_end..node_store_end])?,
                PropertyIndexNodeStore::decode_paged_area(&bytes[node_store_end..edge_store_end])?,
            ),
            other => return Err(PropertyIndexError::UnsupportedVersion(other)),
        };
        Ok(Self {
            snapshot,
            node_store,
            edge_store,
        })
    }
}

mod storage;

pub use storage::{
    read_edge_property_index_node_record_from_stable_memory,
    read_edge_property_index_paged_area_from_stable_memory,
    read_node_property_index_node_record_from_stable_memory,
    read_node_property_index_paged_area_from_stable_memory,
    read_property_index_region_header_from_stable_memory,
    read_property_index_snapshot_from_stable_memory,
    read_property_index_snapshot_section_from_stable_memory,
    read_property_index_storage_image_from_stable_memory,
    scan_edge_property_index_property_prefix_from_stable_memory,
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
    write_property_index_paged_stores_to_stable_memory,
    write_property_index_snapshot_to_stable_memory,
    write_property_index_storage_image_to_stable_memory,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::low_level::{BucketSizeInPages, RegionKind, RegionManager};
    use crate::property_store::default_property_region_chain;
    use crate::stable::{Storable, VecMemory};
    use gleaph_graph_kernel::NodeId;

    #[test]
    fn property_index_header_round_trips_fixed_width_encoding() {
        let header = PropertyIndexHeader {
            root: PropertyIndexNodeId(11),
            first_leaf: PropertyIndexNodeId(12),
            last_leaf: PropertyIndexNodeId(14),
            entry_count: 5,
            branching_factor: 64,
            layout_version: PropertyIndexHeader::CURRENT_LAYOUT_VERSION,
            reserved: 0,
        };
        let decoded = PropertyIndexHeader::decode(&header.encode()).expect("decode header");
        assert_eq!(decoded, header);
    }

    #[test]
    fn property_index_key_round_trips_through_storable_bytes() {
        let key = PropertyIndexKey::node(NodeId::from(7u8), "uid", b"u7".to_vec());
        let restored = PropertyIndexKey::from_bytes(key.to_bytes());
        assert_eq!(restored, key);
    }

    #[test]
    fn property_index_key_groups_by_property_then_value_then_entity() {
        let a = PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a".to_vec());
        let b = PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a".to_vec());
        let c = PropertyIndexKey::node(NodeId::from(3u8), "uid", b"b".to_vec());
        let d = PropertyIndexKey::node(NodeId::from(4u8), "weight", b"1".to_vec());
        assert!(a < b);
        assert!(b < c);
        assert!(a < d);
    }

    #[test]
    fn property_index_scans_exact_value_prefix() {
        let mut index = PropertyIndex::new(64);
        index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        index.insert(
            PropertyIndexKey::node(NodeId::from(2u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        index.insert(
            PropertyIndexKey::node(NodeId::from(3u8), "uid", b"bob".to_vec()),
            PropertyIndexEntry::empty(),
        );

        let matches = index.scan_value_prefix(PropertyIndexEntityKind::VertexNode, "uid", b"alice");
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].0.entity_id, 1);
        assert_eq!(matches[1].0.entity_id, 2);
    }

    #[test]
    fn property_index_tracks_entry_count() {
        let mut index = PropertyIndex::new(64);
        let key = PropertyIndexKey::edge(10, "weight", 7_i64.to_be_bytes().to_vec());
        index.insert(key.clone(), PropertyIndexEntry::empty());
        assert_eq!(index.header.entry_count, 1);
        index.insert(key.clone(), PropertyIndexEntry::empty());
        assert_eq!(index.header.entry_count, 1);
        index.remove(&key);
        assert_eq!(index.header.entry_count, 0);
    }

    #[test]
    fn property_index_allocator_header_round_trips_fixed_width_encoding() {
        let header = PropertyIndexAllocatorHeader {
            next_node_id: 17,
            free_list_head: PropertyIndexNodeId(9),
            page_size_bytes: 4096,
            reserved: 0,
        };
        let decoded = PropertyIndexAllocatorHeader::decode(&header.encode())
            .expect("decode allocator header");
        assert_eq!(decoded, header);
    }

    #[test]
    fn property_index_leaf_node_record_round_trips() {
        let record = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(11),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry {
                        payload: vec![1, 2, 3],
                    },
                ),
            ],
        };
        let decoded = PropertyIndexNodeRecord::decode(&record.encode().expect("encode leaf"))
            .expect("decode leaf");
        assert_eq!(decoded, record);
    }

    #[test]
    fn property_index_node_store_reuses_freed_node_ids_in_lifo_order() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let first = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                0,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: Vec::new(),
        });
        let second = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                0,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: Vec::new(),
        });
        assert_eq!(first, PropertyIndexNodeId(1));
        assert_eq!(second, PropertyIndexNodeId(2));

        store.free(first).expect("free first");
        store.free(second).expect("free second");
        assert_eq!(store.allocator.free_list_head, second);

        let reused = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                0,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: Vec::new(),
        });
        assert_eq!(reused, second);
    }

    #[test]
    fn property_index_node_store_round_trips_snapshot_image() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf_id = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let internal_id = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"bob".to_vec(),
            )],
            children: vec![leaf_id, PropertyIndexNodeId(99)],
        });
        store.free(internal_id).expect("free internal");

        let restored = PropertyIndexNodeStore::decode(&store.encode().expect("encode store"))
            .expect("decode store");
        assert_eq!(restored, store);
    }

    #[test]
    fn property_index_node_store_round_trips_paged_area() {
        let mut store = PropertyIndexNodeStore::new(256);
        let first = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let second = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        store.free(first).expect("free first");
        assert_eq!(second, PropertyIndexNodeId(2));

        let restored = PropertyIndexNodeStore::decode_paged_area(
            &store.encode_paged_area().expect("encode paged area"),
        )
        .expect("decode paged area");
        assert_eq!(restored, store);
    }

    #[test]
    fn property_index_paged_area_tail_extend_matches_full_encode_when_overflow_absent() {
        let mut store = PropertyIndexNodeStore::new(512);
        let _a = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let old = store.encode_paged_area().expect("encode");
        let _b = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"b".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let full = store.encode_paged_area().expect("full");
        let ext = store
            .try_encode_paged_area_zero_overflow_tail_extend(&old)
            .expect("try tail extend")
            .expect("tail extend path");
        assert_eq!(full, ext);
    }

    #[test]
    fn property_index_paged_area_incremental_matches_full_encode_after_local_edit() {
        let mut store = PropertyIndexNodeStore::new(512);
        let leaf_id = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let old = store.encode_paged_area().expect("encode");
        match store.get_mut(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { entries, .. }) => {
                entries[0].0 =
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice2".to_vec());
            }
            _ => panic!("expected leaf"),
        }
        let full = store.encode_paged_area().expect("full");
        let inc = store
            .try_encode_paged_area_incremental(&old)
            .expect("try inc")
            .expect("incremental path");
        assert_eq!(full, inc);
    }

    #[test]
    fn property_index_node_page_round_trips_fixed_size_encoding() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let record = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        };

        let page = store.encode_node_page(&record).expect("encode page");
        assert_eq!(page.len(), 4096);
        let decoded = store.decode_node_page(&page).expect("decode page");
        assert_eq!(decoded, record);
        let node_id = store.allocate(record);
        assert_eq!(store.node_page_offset(node_id).expect("page offset"), 0);
    }

    #[test]
    fn property_index_node_pages_can_round_trip_across_overflow_pages() {
        let store = PropertyIndexNodeStore::new(128);
        let record = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", vec![0u8; 256]),
                PropertyIndexEntry::empty(),
            )],
        };

        let pages = store.encode_node_pages(&record).expect("encode pages");
        assert!(pages.len() > 1);
        let decoded = store.decode_node_pages(&pages).expect("decode pages");
        assert_eq!(decoded, record);
    }

    #[test]
    fn property_index_single_page_encoding_rejects_records_larger_than_page() {
        let store = PropertyIndexNodeStore::new(128);
        let record = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", vec![0u8; 256]),
                PropertyIndexEntry::empty(),
            )],
        };

        match store.encode_node_page(&record) {
            Err(PropertyIndexError::NodeTooLarge { .. }) => {}
            other => panic!("expected NodeTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn property_index_node_store_can_be_derived_from_logical_index() {
        let mut index = PropertyIndex::new(64);
        index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        index.insert(
            PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
            PropertyIndexEntry::empty(),
        );

        let store = PropertyIndexNodeStore::try_from_index(&index, 4096).unwrap();
        assert_eq!(store.nodes.len(), 1);
        let (&node_id, record) = store.nodes.iter().next().expect("single node");
        assert_eq!(node_id, PropertyIndexNodeId(1));
        match record {
            PropertyIndexNodeRecord::Leaf { entries, .. } => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].0.entity_id, 1);
                assert_eq!(entries[1].0.entity_id, 2);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
    }

    fn test_leaf(prev: PropertyIndexNodeId, next: PropertyIndexNodeId) -> PropertyIndexNodeRecord {
        PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(0, prev, next),
            entries: Vec::new(),
        }
    }

    #[test]
    fn incremental_leaf_chain_shape_empty_store_ok() {
        let store = PropertyIndexNodeStore::new(4096);
        assert_eq!(
            store.try_incremental_leaf_chain_shape().unwrap(),
            (Vec::new(), Vec::new(), 2),
        );
    }

    #[test]
    fn incremental_leaf_chain_shape_ok_from_index_multi_leaf() {
        let mut index = PropertyIndex::new(64);
        for (id, byte) in [(1u8, b'a'), (2u8, b'b'), (3u8, b'c')] {
            index.insert(
                PropertyIndexKey::node(NodeId::from(id), "uid", vec![byte; 96]),
                PropertyIndexEntry::empty(),
            );
        }
        let store = PropertyIndexNodeStore::try_from_index(&index, 192).unwrap();
        assert!(store.try_incremental_leaf_chain_shape().is_ok());
    }

    #[test]
    fn incremental_leaf_chain_shape_detects_next_leaf_cycle() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let a = store.allocate(test_leaf(
            PropertyIndexNodeId::NULL,
            PropertyIndexNodeId::NULL,
        ));
        let b = store.allocate(test_leaf(a, a));
        store.nodes.insert(
            a,
            PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(0, PropertyIndexNodeId::NULL, b),
                entries: Vec::new(),
            },
        );
        assert_eq!((a, b), (PropertyIndexNodeId(1), PropertyIndexNodeId(2)));
        assert_eq!(
            store.try_incremental_leaf_chain_shape(),
            Err(PropertyIndexLeafChainShapeError::NextLeafCycle { at: a }),
        );
    }

    #[test]
    fn incremental_leaf_chain_shape_detects_incomplete_chain() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let a = store.allocate(test_leaf(
            PropertyIndexNodeId::NULL,
            PropertyIndexNodeId::NULL,
        ));
        let b = store.allocate(test_leaf(a, PropertyIndexNodeId::NULL));
        store.nodes.insert(
            a,
            PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(0, PropertyIndexNodeId::NULL, b),
                entries: Vec::new(),
            },
        );
        let _c = store.allocate(test_leaf(
            PropertyIndexNodeId::NULL,
            PropertyIndexNodeId::NULL,
        ));
        assert_eq!(store.leaf_node_ids().len(), 3);
        let err = store.try_incremental_leaf_chain_shape().unwrap_err();
        match err {
            PropertyIndexLeafChainShapeError::NextLeafChainLenMismatch {
                visited,
                expected: 3,
            } => assert!(visited < 3),
            other => panic!("expected len mismatch, got {other:?}"),
        }
    }

    #[test]
    fn incremental_leaf_chain_shape_detects_next_to_missing_node() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let ghost = PropertyIndexNodeId(999);
        let _a = store.allocate(test_leaf(PropertyIndexNodeId::NULL, ghost));
        assert_eq!(
            store.try_incremental_leaf_chain_shape(),
            Err(PropertyIndexLeafChainShapeError::NextLeafNotLeaf { at: ghost }),
        );
    }

    #[test]
    fn incremental_leaf_chain_shape_detects_unreachable_leftmost_under_internal() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(0, 1),
            keys: vec![],
            children: vec![PropertyIndexNodeId(1)],
        });
        assert_eq!(
            store.try_incremental_leaf_chain_shape(),
            Err(PropertyIndexLeafChainShapeError::InternalLeftmostLeafUnreachable { root }),
        );
    }

    #[test]
    fn property_index_node_store_from_index_can_build_leaf_chain_and_internal_root() {
        let mut index = PropertyIndex::new(64);
        for (id, value) in [
            (1u8, "alice"),
            (2u8, "bob"),
            (3u8, "carol"),
            (4u8, "dave"),
            (5u8, "erin"),
            (6u8, "frank"),
        ] {
            index.insert(
                PropertyIndexKey::node(NodeId::from(id), "uid", vec![value.as_bytes()[0]; 96]),
                PropertyIndexEntry::empty(),
            );
        }

        let store = PropertyIndexNodeStore::try_from_index(&index, 192).unwrap();
        let restored = store.to_index(64);
        assert_eq!(restored.entries, index.entries);
        assert!(store.nodes.len() >= 3);
        assert_ne!(restored.header.root, restored.header.first_leaf);
        match store.get(restored.header.root).expect("internal root") {
            PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                assert!(!keys.is_empty());
                assert_eq!(children.len(), keys.len() + 1);
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        }
    }

    #[test]
    fn property_index_node_store_from_index_can_build_multi_level_internal_shape() {
        let mut index = PropertyIndex::new(2);
        for (id, byte) in [
            (1u8, b'a'),
            (2u8, b'b'),
            (3u8, b'c'),
            (4u8, b'd'),
            (5u8, b'e'),
            (6u8, b'f'),
            (7u8, b'g'),
            (8u8, b'h'),
        ] {
            index.insert(
                PropertyIndexKey::node(NodeId::from(id), "uid", vec![byte; 96]),
                PropertyIndexEntry::empty(),
            );
        }

        let store = PropertyIndexNodeStore::try_from_index(&index, 192).unwrap();
        let restored = store.to_index(2);
        assert_eq!(restored.entries, index.entries);
        assert_ne!(restored.header.root, restored.header.first_leaf);

        let root = store.get(restored.header.root).expect("root node");
        let root_children = match root {
            PropertyIndexNodeRecord::Internal { children, .. } => children,
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        };
        assert!(
            root_children.iter().any(|child_id| matches!(
                store.get(*child_id),
                Some(PropertyIndexNodeRecord::Internal { .. })
            )),
            "expected at least one internal child beneath the root",
        );
    }

    #[test]
    fn property_index_node_store_can_reconstruct_logical_index() {
        let mut index = PropertyIndex::new(64);
        index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        index.insert(
            PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
            PropertyIndexEntry { payload: vec![7] },
        );

        let store = PropertyIndexNodeStore::try_from_index(&index, 4096).unwrap();
        let restored = store.to_index(64);
        assert_eq!(restored.entries, index.entries);
        assert_eq!(restored.header.entry_count, index.header.entry_count);
        assert_eq!(restored.header.root, PropertyIndexNodeId(1));
        assert_eq!(restored.header.first_leaf, PropertyIndexNodeId(1));
        assert_eq!(restored.header.last_leaf, PropertyIndexNodeId(1));
    }

    #[test]
    fn property_index_node_store_reconstructs_from_leaf_chain_metadata() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let second = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId(1),
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let first = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, PropertyIndexNodeId::NULL, second),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });

        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(second) {
            header.prev_leaf = first;
        }

        let restored = store.to_index(64);
        assert_eq!(restored.header.root, first);
        assert_eq!(restored.header.first_leaf, first);
        assert_eq!(restored.header.last_leaf, second);
        let keys: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(keys, vec![1, 2]);
    }

    #[test]
    fn property_index_node_store_reconstructs_from_internal_root_to_leaf_chain() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"bob".to_vec(),
            )],
            children: vec![left, right],
        });

        let restored = store.to_index(64);
        assert_eq!(restored.header.root, root);
        assert_eq!(restored.header.first_leaf, left);
        assert_eq!(restored.header.last_leaf, right);
        let keys: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(keys, vec![1, 2]);
    }

    #[test]
    fn property_index_node_store_can_scan_exact_value_prefix_directly() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"bob".to_vec(),
            )],
            children: vec![left, right],
        });

        let alice =
            store.scan_value_prefix_direct(PropertyIndexEntityKind::VertexNode, "uid", b"alice");
        let bob =
            store.scan_value_prefix_direct(PropertyIndexEntityKind::VertexNode, "uid", b"bob");

        assert_eq!(alice.len(), 1);
        assert_eq!(alice[0].0.entity_id, 1);
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].0.entity_id, 2);
    }

    #[test]
    fn property_index_node_store_can_scan_property_prefix_directly() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "name", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"bob".to_vec(),
            )],
            children: vec![left, right],
        });

        let uid = store.scan_property_prefix_direct(PropertyIndexEntityKind::VertexNode, "uid");
        assert_eq!(uid.len(), 2);
        let ids: Vec<_> = uid.into_iter().map(|(key, _)| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn property_index_node_store_can_upsert_and_remove_in_single_leaf_mode() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let alice = PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec());
        let bob = PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec());

        assert!(store.upsert_single_leaf_entry(alice.clone(), PropertyIndexEntry::empty()));
        assert!(store.upsert_single_leaf_entry(bob.clone(), PropertyIndexEntry::empty()));
        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2]);

        assert!(store.remove_single_leaf_entry(&alice));
        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![2]);

        assert!(store.remove_single_leaf_entry(&bob));
        assert!(store.nodes.is_empty());
        assert_eq!(store.allocator.next_node_id, 1);
        assert!(store.free_node_ids.is_empty());
    }

    #[test]
    fn property_index_node_store_can_upsert_across_leaf_chain_without_internal_nodes() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(5u8), "uid", b"erin".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        assert_eq!(restored.header.first_leaf, left);
        let mut leaf = restored.header.first_leaf;
        let mut leaf_counts = Vec::new();
        while leaf != PropertyIndexNodeId::NULL {
            match store.get(leaf).expect("leaf in chain") {
                PropertyIndexNodeRecord::Leaf { header, .. } => {
                    assert!(header.entry_count > 0);
                    leaf_counts.push(header.entry_count);
                    leaf = header.next_leaf;
                }
                PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
            }
        }
        assert!(leaf_counts.len() >= 2);
        assert_eq!(leaf_counts.iter().copied().sum::<u16>(), 5);
    }

    #[test]
    fn property_index_node_store_can_remove_across_leaf_chain_without_internal_nodes() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(1u8),
            "uid",
            b"alice".to_vec(),
        )));
        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(2u8),
            "uid",
            b"bob".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![3, 4]);
        assert!(matches!(
            store.get(restored.header.first_leaf),
            Some(PropertyIndexNodeRecord::Leaf { .. })
        ));
        assert!(
            store
                .nodes
                .values()
                .filter_map(|record| match record {
                    PropertyIndexNodeRecord::Leaf { entries, .. } => Some(entries.len()),
                    PropertyIndexNodeRecord::Internal { .. } => None,
                })
                .any(|entry_len| entry_len == 2)
        );
    }

    #[test]
    fn property_index_node_store_can_upsert_across_leaf_chain_with_single_internal_root() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(3u8),
                "uid",
                b"carol".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(5u8), "uid", b"erin".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        assert_ne!(restored.header.root, left);
        assert_ne!(restored.header.root, right);
        match store.get(restored.header.root).expect("root internal") {
            PropertyIndexNodeRecord::Internal {
                header,
                keys,
                children,
            } => {
                assert_eq!(header.entry_count as usize, keys.len());
                assert_eq!(children.len(), keys.len() + 1);
                assert!(
                    children.iter().any(|child_id| matches!(
                        store.get(*child_id),
                        Some(PropertyIndexNodeRecord::Leaf { .. })
                    )),
                    "expected root to route to leaf-level subtrees",
                );
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        }
    }

    #[test]
    fn property_index_node_store_can_collapse_internal_root_after_leaf_chain_removal() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(3u8),
                "uid",
                b"carol".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(1u8),
            "uid",
            b"alice".to_vec(),
        )));
        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(2u8),
            "uid",
            b"bob".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![3, 4]);
        assert_eq!(store.nodes.len(), 1);
        let _ = root;
        assert!(matches!(
            store.get(restored.header.first_leaf),
            Some(PropertyIndexNodeRecord::Leaf { .. })
        ));
        assert!(matches!(
            store.get(restored.header.last_leaf),
            Some(PropertyIndexNodeRecord::Leaf { .. })
        ));
        assert_eq!(restored.header.root, restored.header.first_leaf);
        assert_eq!(restored.header.first_leaf, restored.header.last_leaf);
    }

    #[test]
    fn property_index_node_store_can_merge_underfull_leaf_after_removal() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(3u8),
                "uid",
                b"carol".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(2u8),
            "uid",
            b"bob".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 3]);
        assert!(store.get(right).is_none());
        assert_eq!(store.nodes.len(), 1);
        assert_eq!(restored.header.root, left);
        assert_eq!(restored.header.first_leaf, left);
        assert_eq!(restored.header.last_leaf, left);
        match store.get(left).expect("merged leaf") {
            PropertyIndexNodeRecord::Leaf { header, entries } => {
                assert_eq!(header.entry_count, 2);
                let ids: Vec<_> = entries.iter().map(|(key, _)| key.entity_id).collect();
                assert_eq!(ids, vec![1, 3]);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
    }

    #[test]
    fn property_index_node_store_can_redistribute_underfull_leaf_after_removal() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(3, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"a4".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"a5".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(3u8),
                "uid",
                b"a3".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(1u8),
            "uid",
            b"a1".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![2, 3, 4, 5]);
        assert_eq!(restored.header.root, root);
        assert_eq!(store.nodes.len(), 3);
        match store.get(left).expect("left leaf") {
            PropertyIndexNodeRecord::Leaf { entries, header } => {
                assert_eq!(header.next_leaf, right);
                let ids: Vec<_> = entries.iter().map(|(key, _)| key.entity_id).collect();
                assert_eq!(ids, vec![2, 3]);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
        match store.get(right).expect("right leaf") {
            PropertyIndexNodeRecord::Leaf { entries, header } => {
                assert_eq!(header.prev_leaf, left);
                let ids: Vec<_> = entries.iter().map(|(key, _)| key.entity_id).collect();
                assert_eq!(ids, vec![4, 5]);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
        match store.get(root).expect("root internal") {
            PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                assert_eq!(children, &vec![left, right]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(right).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_node_store_can_reuse_single_internal_root_after_middle_leaf_collapse() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let middle = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId(3)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, middle, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = middle;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(middle) {
            header.next_leaf = right;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
            ],
            children: vec![left, middle, right],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(3u8),
            "uid",
            b"carol".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 4]);
        assert!(store.get(middle).is_none());
        assert_eq!(restored.header.root, root);
        match store.get(root).expect("reused root") {
            PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                assert_eq!(children, &vec![left, right]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0].entity_id, 4);
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        }
    }

    #[test]
    fn property_index_node_store_can_reuse_single_internal_root_after_local_leaf_split() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"erin".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"dave".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
        assert_eq!(restored.header.root, root);
        match store.get(root).expect("reused root") {
            PropertyIndexNodeRecord::Internal {
                header,
                keys,
                children,
            } => {
                assert_eq!(header.capacity, 3);
                assert!((2..=3).contains(&children.len()));
                assert_eq!(keys.len(), children.len() - 1);
                for (key, child_id) in keys.iter().zip(children.iter().skip(1)) {
                    assert_eq!(*key, store.first_key_for_subtree(*child_id).unwrap());
                }
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        }
    }

    #[test]
    fn property_index_node_store_can_attach_split_leaf_to_parent_in_multi_level_shape() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf1, PropertyIndexNodeId(3)),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"erin".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf2, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(6u8), "uid", b"frank".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(7u8), "uid", b"grace".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf1) {
            header.next_leaf = leaf2;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf2) {
            header.next_leaf = leaf3;
        }

        let internal_left = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"dave".to_vec(),
            )],
            children: vec![leaf1, leaf2],
        });
        let internal_right = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(6u8),
                "uid",
                b"frank".to_vec(),
            )],
            children: vec![leaf3, leaf3],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(6u8),
                "uid",
                b"frank".to_vec(),
            )],
            children: vec![internal_left, internal_right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(restored.header.root, root);
        match store.get(internal_left).expect("left internal") {
            PropertyIndexNodeRecord::Internal {
                header,
                keys,
                children,
            } => {
                assert_eq!(header.capacity, 3);
                assert!((2..=3).contains(&children.len()));
                assert_eq!(keys.len(), children.len() - 1);
                for (key, child_id) in keys.iter().zip(children.iter().skip(1)) {
                    assert_eq!(*key, store.first_key_for_subtree(*child_id).unwrap());
                }
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
        match store.get(root).expect("root internal") {
            PropertyIndexNodeRecord::Internal { children, .. } => {
                assert_eq!(children, &vec![internal_left, internal_right]);
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_node_store_can_attach_split_leaf_via_parent_split_to_grandparent() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf1, PropertyIndexNodeId(3)),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"b1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"b2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf2, PropertyIndexNodeId(4)),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(6u8), "uid", b"c1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(7u8), "uid", b"c2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf4 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf3, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(9u8), "uid", b"d1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(10u8), "uid", b"d2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf1) {
            header.next_leaf = leaf2;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf2) {
            header.next_leaf = leaf3;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf3) {
            header.next_leaf = leaf4;
        }

        let internal_left = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"b1".to_vec()),
                PropertyIndexKey::node(NodeId::from(6u8), "uid", b"c1".to_vec()),
            ],
            children: vec![leaf1, leaf2, leaf3],
        });
        let internal_right = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(9u8),
                "uid",
                b"d1".to_vec(),
            )],
            children: vec![leaf4, leaf4],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(9u8),
                "uid",
                b"d1".to_vec(),
            )],
            children: vec![internal_left, internal_right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7, 9, 10]);
        assert_eq!(restored.header.root, root);
        match store.get(root).expect("root internal") {
            PropertyIndexNodeRecord::Internal {
                header,
                children,
                keys,
            } => {
                assert_eq!(header.capacity, 3);
                assert!((2..=3).contains(&children.len()));
                assert_eq!(keys.len(), children.len() - 1);
                for (key, child_id) in keys.iter().zip(children.iter().skip(1)) {
                    assert_eq!(*key, store.first_key_for_subtree(*child_id).unwrap());
                }
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_node_store_can_compact_ancestors_after_empty_leaf_collapse() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a1".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf1, PropertyIndexNodeId(3)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a2".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf2, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"b1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"b2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf1) {
            header.next_leaf = leaf2;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf2) {
            header.next_leaf = leaf3;
        }

        let left_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"a2".to_vec(),
            )],
            children: vec![leaf1, leaf2],
        });
        let right_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(5u8),
                "uid",
                b"b2".to_vec(),
            )],
            children: vec![leaf3, leaf3],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"b1".to_vec(),
            )],
            children: vec![left_internal, right_internal],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(2u8),
            "uid",
            b"a2".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 4, 5]);
        assert!(store.get(leaf2).is_none());
        assert!(store.get(left_internal).is_none());
        assert_eq!(restored.header.root, root);
        match store.get(root).expect("root internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children.len(), 2);
                assert_eq!(children[0], leaf1);
                assert_eq!(children[1], right_internal);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        }
    }

    #[test]
    fn property_index_node_store_can_compact_parent_after_right_leaf_merge() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf1, PropertyIndexNodeId(3)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf2, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"b1".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(6u8), "uid", b"b2".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf1) {
            header.next_leaf = leaf2;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf2) {
            header.next_leaf = leaf3;
        }

        let parent = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                PropertyIndexKey::node(NodeId::from(5u8), "uid", b"b1".to_vec()),
            ],
            children: vec![leaf1, leaf2, leaf3],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(1u8),
                "uid",
                b"a1".to_vec(),
            )],
            children: vec![parent, parent],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(3u8),
            "uid",
            b"a3".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 5, 6]);
        assert!(store.get(leaf2).is_none());
        assert_eq!(restored.header.root, root);
        match store.get(parent).expect("reused parent") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children.len(), 2);
                assert_eq!(children, &vec![leaf1, leaf3]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal parent"),
        }
    }

    #[test]
    fn property_index_node_store_can_borrow_for_underfull_internal_after_leaf_collapse() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a1".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf1, PropertyIndexNodeId(3)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a2".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf2, PropertyIndexNodeId(4)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf4 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf3, PropertyIndexNodeId(5)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"a4".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf5 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf4, PropertyIndexNodeId(6)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(5u8), "uid", b"a5".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf6 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf5, PropertyIndexNodeId(7)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(6u8), "uid", b"a6".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf7 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf6, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(7u8), "uid", b"a7".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        for (left, right) in [
            (leaf1, leaf2),
            (leaf2, leaf3),
            (leaf3, leaf4),
            (leaf4, leaf5),
            (leaf5, leaf6),
            (leaf6, leaf7),
        ] {
            if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
                header.next_leaf = right;
            }
        }

        let left_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"a2".to_vec(),
            )],
            children: vec![leaf1, leaf2],
        });
        let middle_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"a4".to_vec(),
            )],
            children: vec![leaf3, leaf4],
        });
        let right_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(6u8), "uid", b"a6".to_vec()),
                PropertyIndexKey::node(NodeId::from(7u8), "uid", b"a7".to_vec()),
            ],
            children: vec![leaf5, leaf6, leaf7],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                PropertyIndexKey::node(NodeId::from(5u8), "uid", b"a5".to_vec()),
            ],
            children: vec![left_internal, middle_internal, right_internal],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(3u8),
            "uid",
            b"a3".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 4, 5, 6, 7]);
        assert_eq!(restored.header.root, root);
        match store.get(middle_internal).expect("middle internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children, &vec![leaf4, leaf5]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
        match store.get(right_internal).expect("right internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children, &vec![leaf6, leaf7]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_node_store_can_merge_underfull_internal_after_leaf_collapse() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a1".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf1, PropertyIndexNodeId(3)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"a2".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf2, PropertyIndexNodeId(4)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf4 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf3, PropertyIndexNodeId(5)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"a4".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf5 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf4, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(5u8), "uid", b"a5".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        for (left, right) in [
            (leaf1, leaf2),
            (leaf2, leaf3),
            (leaf3, leaf4),
            (leaf4, leaf5),
        ] {
            if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
                header.next_leaf = right;
            }
        }

        let left_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"a2".to_vec(),
            )],
            children: vec![leaf1, leaf2],
        });
        let middle_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"a4".to_vec(),
            )],
            children: vec![leaf3, leaf4],
        });
        let right_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(5u8),
                "uid",
                b"a5".to_vec(),
            )],
            children: vec![leaf5, leaf5],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"a3".to_vec()),
                PropertyIndexKey::node(NodeId::from(5u8), "uid", b"a5".to_vec()),
            ],
            children: vec![left_internal, middle_internal, right_internal],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(3u8),
            "uid",
            b"a3".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 4, 5]);
        assert_eq!(restored.header.root, root);
        assert!(store.get(right_internal).is_none());
        match store.get(middle_internal).expect("merged internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children.len(), 3);
                assert_eq!(children[0], leaf4);
                assert_eq!(keys.len(), 2);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
                assert_eq!(keys[1], store.first_key_for_subtree(children[2]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
        match store.get(root).expect("root internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children, &vec![left_internal, middle_internal]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_node_store_can_propagate_internal_underflow_repair_to_ancestor() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let mut leaves = Vec::new();
        for id in 1u8..=10 {
            let prev = leaves.last().copied().unwrap_or(PropertyIndexNodeId::NULL);
            let leaf = store.allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(1, prev, PropertyIndexNodeId::NULL),
                entries: vec![(
                    PropertyIndexKey::node(
                        NodeId::from(id),
                        "uid",
                        format!("a{id:02}").into_bytes(),
                    ),
                    PropertyIndexEntry::empty(),
                )],
            });
            if let Some(previous) = leaves.last().copied()
                && let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(previous)
            {
                header.next_leaf = leaf;
            }
            leaves.push(leaf);
        }

        let left_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"a02".to_vec(),
            )],
            children: vec![leaves[0], leaves[1]],
        });
        let middle_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"a04".to_vec(),
            )],
            children: vec![leaves[2], leaves[3]],
        });
        let right_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(6u8),
                "uid",
                b"a06".to_vec(),
            )],
            children: vec![leaves[4], leaves[5]],
        });
        let far_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(8u8),
                "uid",
                b"a08".to_vec(),
            )],
            children: vec![leaves[6], leaves[7]],
        });
        let farthest_internal = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(10u8),
                "uid",
                b"a10".to_vec(),
            )],
            children: vec![leaves[8], leaves[9]],
        });

        let upper_left = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![store.first_key_for_subtree(middle_internal).unwrap()],
            children: vec![left_internal, middle_internal],
        });
        let upper_right = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 3),
            keys: vec![
                store.first_key_for_subtree(far_internal).unwrap(),
                store.first_key_for_subtree(farthest_internal).unwrap(),
            ],
            children: vec![right_internal, far_internal, farthest_internal],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 2),
            keys: vec![store.first_key_for_subtree(upper_right).unwrap()],
            children: vec![upper_left, upper_right],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(3u8),
            "uid",
            b"a03".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(restored.header.root, root);
        assert!(store.get(middle_internal).is_none());
        match store.get(upper_left).expect("upper left internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children, &vec![left_internal, right_internal]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
        match store.get(upper_right).expect("upper right internal") {
            PropertyIndexNodeRecord::Internal { children, keys, .. } => {
                assert_eq!(children, &vec![far_internal, farthest_internal]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(children[1]).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_node_store_can_upsert_across_multi_level_internal_shape() {
        let mut index = PropertyIndex::new(2);
        for (id, byte) in [
            (1u8, b'a'),
            (2u8, b'b'),
            (3u8, b'c'),
            (4u8, b'd'),
            (5u8, b'e'),
            (6u8, b'f'),
            (7u8, b'g'),
            (8u8, b'h'),
        ] {
            index.insert(
                PropertyIndexKey::node(NodeId::from(id), "uid", vec![byte; 96]),
                PropertyIndexEntry::empty(),
            );
        }
        let mut store = PropertyIndexNodeStore::try_from_index(&index, 192).unwrap();

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(9u8), "uid", vec![b'i'; 96]),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(2);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
        match store.get(restored.header.root).expect("root node") {
            PropertyIndexNodeRecord::Internal { children, .. } => {
                assert!(
                    children.iter().any(|child_id| matches!(
                        store.get(*child_id),
                        Some(PropertyIndexNodeRecord::Internal { .. })
                    )),
                    "expected multi-level internal shape to remain after upsert",
                );
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal root"),
        }
    }

    #[test]
    fn property_index_node_store_can_upsert_locally_without_rebuilding_single_internal_root() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"dave".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(5u8), "uid", b"erin".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        assert_eq!(store.nodes.len(), 3);
        let restored = store.to_index(64);
        assert_eq!(restored.header.root, root);
        match store.get(right).expect("right leaf") {
            PropertyIndexNodeRecord::Leaf { entries, .. } => {
                let ids: Vec<_> = entries.iter().map(|(key, _)| key.entity_id).collect();
                assert_eq!(ids, vec![4, 5]);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
    }

    #[test]
    fn property_index_node_store_can_redistribute_insert_across_adjacent_leaves() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(1, 3),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(4u8),
                "uid",
                b"dave".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(restored.header.root, root);
        assert_eq!(store.nodes.len(), 3);
        match store.get(left).expect("left leaf") {
            PropertyIndexNodeRecord::Leaf { entries, header } => {
                assert_eq!(header.next_leaf, right);
                let ids: Vec<_> = entries.iter().map(|(key, _)| key.entity_id).collect();
                assert_eq!(ids, vec![1, 2]);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
        match store.get(right).expect("right leaf") {
            PropertyIndexNodeRecord::Leaf { entries, header } => {
                assert_eq!(header.prev_leaf, left);
                let ids: Vec<_> = entries.iter().map(|(key, _)| key.entity_id).collect();
                assert_eq!(ids, vec![3, 4]);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
        match store.get(root).expect("root internal") {
            PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                assert_eq!(children, &vec![left, right]);
                assert_eq!(keys.len(), 1);
                assert_eq!(keys[0], store.first_key_for_subtree(right).unwrap());
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    /// Two-leaf redistribution cannot always find an encoding-safe split when both adjacent
    /// leaves are tight; merging three siblings and repartitioning must succeed locally.
    #[test]
    fn property_index_node_store_can_redistribute_insert_across_three_leaves() {
        // Page budget must fit the repacked chunks from `partition_entries_into_leaf_chunks`
        // while still keeping the initial two-entry leaves full enough that pairwise merge fails.
        let page = 512u32;
        let mut store = PropertyIndexNodeStore::new(page);
        let mk = |id: u8| -> PropertyIndexKey {
            PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; 48])
        };
        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (mk(1), PropertyIndexEntry::empty()),
                (mk(2), PropertyIndexEntry::empty()),
            ],
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, l0, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(3), PropertyIndexEntry::empty()),
                (mk(4), PropertyIndexEntry::empty()),
            ],
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, l1, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(5), PropertyIndexEntry::empty()),
                (mk(6), PropertyIndexEntry::empty()),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 4),
            keys: vec![mk(3), mk(5)],
            children: vec![l0, l1, l2],
        });

        let mut between_3_and_4 = vec![3u8; 48];
        between_3_and_4[47] = 4;
        let insert_key = PropertyIndexKey::node(NodeId::from(7u8), "uid", between_3_and_4);
        assert_eq!(
            store.upsert_leaf_chain_entry_with_kind(insert_key, PropertyIndexEntry::empty()),
            Some(PropertyIndexNodeStoreMutationKind::Redistribute)
        );

        let restored = store.to_index(64);
        assert_eq!(restored.entries.len(), 7);
        assert_eq!(restored.header.root, root);
        let leaf_count = store
            .nodes
            .values()
            .filter(|r| matches!(r, PropertyIndexNodeRecord::Leaf { .. }))
            .count();
        let internal_count = store
            .nodes
            .values()
            .filter(|r| matches!(r, PropertyIndexNodeRecord::Internal { .. }))
            .count();
        assert_eq!(leaf_count, 3);
        assert_eq!(internal_count, 1);
        for record in store.nodes.values() {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(store.encode_node_page(record).is_ok());
            }
        }
    }

    #[test]
    fn find_leaf_redistribution_split_can_fail_for_both_adjacent_pairwise_merges() {
        let payload = 36usize;
        let mk = |id: u8| -> PropertyIndexKey {
            PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; payload])
        };
        let insert_key = {
            let mut between_5_and_6 = vec![5u8; payload];
            between_5_and_6[payload - 1] = 6;
            PropertyIndexKey::node(NodeId::from(10u8), "uid", between_5_and_6)
        };
        let mut merge_l1_l2 = vec![
            (mk(4), PropertyIndexEntry::empty()),
            (mk(5), PropertyIndexEntry::empty()),
            (mk(6), PropertyIndexEntry::empty()),
            (mk(7), PropertyIndexEntry::empty()),
            (mk(8), PropertyIndexEntry::empty()),
            (mk(9), PropertyIndexEntry::empty()),
        ];
        match merge_l1_l2.binary_search_by(|(k, _)| k.cmp(&insert_key)) {
            Ok(i) => merge_l1_l2[i] = (insert_key.clone(), PropertyIndexEntry::empty()),
            Err(i) => merge_l1_l2.insert(i, (insert_key.clone(), PropertyIndexEntry::empty())),
        }
        let mut merge_l0_l1 = vec![
            (mk(1), PropertyIndexEntry::empty()),
            (mk(2), PropertyIndexEntry::empty()),
            (mk(3), PropertyIndexEntry::empty()),
            (mk(4), PropertyIndexEntry::empty()),
            (mk(5), PropertyIndexEntry::empty()),
            (mk(6), PropertyIndexEntry::empty()),
        ];
        match merge_l0_l1.binary_search_by(|(k, _)| k.cmp(&insert_key)) {
            Ok(i) => merge_l0_l1[i] = (insert_key.clone(), PropertyIndexEntry::empty()),
            Err(i) => merge_l0_l1.insert(i, (insert_key, PropertyIndexEntry::empty())),
        }
        assert_eq!(merge_l1_l2.len(), 7);
        assert_eq!(merge_l0_l1.len(), 7);

        let mut witness_page = None;
        for page in (120u32..=900u32).step_by(2) {
            let store = PropertyIndexNodeStore::new(page);
            let sample = PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    3,
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: vec![
                    (mk(4), PropertyIndexEntry::empty()),
                    (mk(5), PropertyIndexEntry::empty()),
                    (mk(6), PropertyIndexEntry::empty()),
                ],
            };
            if store.encode_node_page(&sample).is_err() {
                continue;
            }
            let no_split_right = store
                .find_leaf_redistribution_split(
                    &merge_l1_l2,
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId(1),
                    PropertyIndexNodeId::NULL,
                )
                .is_none();
            let no_split_left = store
                .find_leaf_redistribution_split(
                    &merge_l0_l1,
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId(1),
                    PropertyIndexNodeId::NULL,
                )
                .is_none();
            if no_split_right && no_split_left {
                witness_page = Some(page);
                break;
            }
        }
        assert!(
            witness_page.is_some(),
            "expected a page where both 7-entry pairwise merges lack a single-page split"
        );
    }

    /// With single-page pairwise splits only, some `(page_size, payload)` pairs force
    /// `try_upsert_three_leaf_redistribute`.
    ///
    /// `PAGE` / `PAYLOAD` are pinned witnesses from the former search (pairwise must fail, three-leaf
    /// must succeed). If node layout or encoding changes, temporarily restore a search loop to
    /// refresh them.
    #[test]
    fn property_index_node_store_upsert_three_leaf_repack_end_to_end() {
        const PAGE: u32 = 184;
        const PAYLOAD: usize = 20;

        let mk = |id: u8| -> PropertyIndexKey {
            PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; PAYLOAD])
        };
        let insert_key = {
            let mut between_5_and_6 = vec![5u8; PAYLOAD];
            between_5_and_6[PAYLOAD - 1] = 6;
            PropertyIndexKey::node(NodeId::from(10u8), "uid", between_5_and_6)
        };

        let mut store = PropertyIndexNodeStore::new(PAGE);
        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                3,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![
                (mk(1), PropertyIndexEntry::empty()),
                (mk(2), PropertyIndexEntry::empty()),
                (mk(3), PropertyIndexEntry::empty()),
            ],
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(3, l0, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(4), PropertyIndexEntry::empty()),
                (mk(5), PropertyIndexEntry::empty()),
                (mk(6), PropertyIndexEntry::empty()),
            ],
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(3, l1, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(7), PropertyIndexEntry::empty()),
                (mk(8), PropertyIndexEntry::empty()),
                (mk(9), PropertyIndexEntry::empty()),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 4),
            keys: vec![mk(4), mk(7)],
            children: vec![l0, l1, l2],
        });

        assert_eq!(
            store.upsert_leaf_chain_entry_with_kind(insert_key, PropertyIndexEntry::empty()),
            Some(PropertyIndexNodeStoreMutationKind::ThreeLeafRepack)
        );
        assert_eq!(store.to_index(64).entries.len(), 10);
        for record in store.nodes.values() {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(store.encode_node_page(record).is_ok());
            }
        }
    }

    /// 3+3+5 leaves with `max`=5 ⇒ `min`=3: deleting from the head leaf (`3`→`2`) underflows it.
    /// The right sibling cannot lend (`len`>`min` is false), so pairwise borrow is skipped and the
    /// full remove path may take `ThreeLeafRepack` (same anchored three-leaf machinery as insert).
    ///
    /// `PAGE` / `PAYLOAD` are pinned witnesses from the former search. If encoding changes,
    /// refresh them the same way as `property_index_node_store_upsert_three_leaf_repack_end_to_end`.
    #[test]
    fn property_index_node_store_remove_three_leaf_repack_after_head_leaf_underflow_end_to_end() {
        const PAGE: u32 = 276;
        const PAYLOAD: usize = 20;

        let mk = |id: u8| -> PropertyIndexKey {
            PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; PAYLOAD])
        };
        let remove_key = mk(2);

        let mut store = PropertyIndexNodeStore::new(PAGE);
        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                3,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![
                (mk(1), PropertyIndexEntry::empty()),
                (mk(2), PropertyIndexEntry::empty()),
                (mk(3), PropertyIndexEntry::empty()),
            ],
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(3, l0, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(4), PropertyIndexEntry::empty()),
                (mk(5), PropertyIndexEntry::empty()),
                (mk(6), PropertyIndexEntry::empty()),
            ],
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(5, l1, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(7), PropertyIndexEntry::empty()),
                (mk(8), PropertyIndexEntry::empty()),
                (mk(9), PropertyIndexEntry::empty()),
                (mk(10), PropertyIndexEntry::empty()),
                (mk(11), PropertyIndexEntry::empty()),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 4),
            keys: vec![mk(4), mk(7)],
            children: vec![l0, l1, l2],
        });

        assert_eq!(
            store.remove_leaf_chain_entry_with_kind(&remove_key),
            Some(PropertyIndexNodeStoreMutationKind::ThreeLeafRepack)
        );
        assert_eq!(store.to_index(64).entries.len(), 10);
        assert!(store.to_index(64).get(&remove_key).is_none());
        for record in store.nodes.values() {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(store.encode_node_page(record).is_ok());
            }
        }
    }

    /// Same pinned `(page, payload)` as
    /// `property_index_node_store_remove_three_leaf_repack_after_head_leaf_underflow_end_to_end`, but
    /// the removable key sits in the **middle** leaf of the first three-leaf window, so
    /// `three_leaf_forward_window_start` must walk back to `l0` before merging.
    ///
    /// A fourth leaf (five entries) lifts `max_leaf_entry_count` so `min` matches the head witness,
    /// while the middle leaf's **right** sibling (`l2`) stays at three entries and cannot lend
    /// (`len > min` is false). The **left** sibling also cannot lend. Pairwise repair therefore
    /// fails and the anchored three-leaf path runs.
    #[test]
    fn property_index_node_store_remove_three_leaf_repack_after_middle_leaf_underflow_end_to_end() {
        const PAGE: u32 = 276;
        const PAYLOAD: usize = 20;

        let mk = |id: u8| -> PropertyIndexKey {
            PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; PAYLOAD])
        };
        let remove_key = mk(5);

        let mut store = PropertyIndexNodeStore::new(PAGE);
        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                3,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: vec![
                (mk(1), PropertyIndexEntry::empty()),
                (mk(2), PropertyIndexEntry::empty()),
                (mk(3), PropertyIndexEntry::empty()),
            ],
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(3, l0, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(4), PropertyIndexEntry::empty()),
                (mk(5), PropertyIndexEntry::empty()),
                (mk(6), PropertyIndexEntry::empty()),
            ],
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(3, l1, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(7), PropertyIndexEntry::empty()),
                (mk(8), PropertyIndexEntry::empty()),
                (mk(9), PropertyIndexEntry::empty()),
            ],
        });
        let l3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(5, l2, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(10), PropertyIndexEntry::empty()),
                (mk(11), PropertyIndexEntry::empty()),
                (mk(12), PropertyIndexEntry::empty()),
                (mk(13), PropertyIndexEntry::empty()),
                (mk(14), PropertyIndexEntry::empty()),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l2) {
            header.next_leaf = l3;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(3, 4),
            keys: vec![mk(4), mk(7), mk(10)],
            children: vec![l0, l1, l2, l3],
        });

        assert_eq!(
            store.remove_leaf_chain_entry_with_kind(&remove_key),
            Some(PropertyIndexNodeStoreMutationKind::ThreeLeafRepack)
        );
        assert_eq!(store.to_index(64).entries.len(), 13);
        assert!(store.to_index(64).get(&remove_key).is_none());
        for record in store.nodes.values() {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(store.encode_node_page(record).is_ok());
            }
        }
    }

    /// Removing from the tail leaf when pairwise borrow fails (merge or three-leaf repack).
    #[test]
    fn property_index_node_store_remove_from_tail_round_trips_with_local_repair() {
        let page = 512u32;
        let mut store = PropertyIndexNodeStore::new(page);
        let mk = |id: u8| -> PropertyIndexKey {
            PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; 48])
        };
        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (mk(1), PropertyIndexEntry::empty()),
                (mk(2), PropertyIndexEntry::empty()),
            ],
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, l0, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(3), PropertyIndexEntry::empty()),
                (mk(4), PropertyIndexEntry::empty()),
            ],
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, l1, PropertyIndexNodeId::NULL),
            entries: vec![
                (mk(5), PropertyIndexEntry::empty()),
                (mk(6), PropertyIndexEntry::empty()),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal_with_capacity(2, 4),
            keys: vec![mk(3), mk(5)],
            children: vec![l0, l1, l2],
        });

        assert_eq!(
            store.remove_leaf_chain_entry_with_kind(&mk(6)),
            Some(PropertyIndexNodeStoreMutationKind::Merge)
        );
        let restored = store.to_index(64);
        assert_eq!(restored.entries.len(), 5);
        assert!(restored.get(&mk(6)).is_none());
        for record in store.nodes.values() {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(store.encode_node_page(record).is_ok());
            }
        }
    }

    #[test]
    fn property_index_node_store_can_split_target_leaf_locally() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let left = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let right = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "uid", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(4u8), "uid", b"dave".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(left) {
            header.next_leaf = right;
        }
        let _root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(3u8),
                "uid",
                b"carol".to_vec(),
            )],
            children: vec![left, right],
        });

        assert!(store.upsert_leaf_chain_entry(
            PropertyIndexKey::node(NodeId::from(6u8), "uid", b"carl".to_vec()),
            PropertyIndexEntry::empty(),
        ));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 6, 3, 4]);
        let leaf_count = store
            .nodes
            .values()
            .filter(|record| matches!(record, PropertyIndexNodeRecord::Leaf { .. }))
            .count();
        assert!((2..=3).contains(&leaf_count));
    }

    /// Replacing an existing binding with a larger payload can require a leaf split even when the
    /// entry count stays constant (`try_upsert_entry_with_leaf_split` must not bail early on `Ok`).
    ///
    /// Uses a **single** leaf in the sibling chain (`prev`/`next` null) under one internal root so
    /// pairwise leaf redistribution cannot absorb the growth — the split path must run.
    #[test]
    fn property_index_node_store_can_split_on_replace_when_payload_grows() {
        let page_size = 512u32;
        let key = PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec());
        let mut chosen = None;
        for n in (200u32..=480).step_by(16) {
            let mut store = PropertyIndexNodeStore::new(page_size);
            let leaf = store.allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    2,
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: vec![
                    (key.clone(), PropertyIndexEntry::empty()),
                    (
                        PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                        PropertyIndexEntry::empty(),
                    ),
                ],
            });
            let _root = store.allocate(PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(0, 4),
                keys: vec![],
                children: vec![leaf],
            });

            let kind = store.upsert_leaf_chain_entry_with_kind(
                key.clone(),
                PropertyIndexEntry {
                    payload: vec![0xabu8; n as usize],
                },
            );
            if kind == Some(PropertyIndexNodeStoreMutationKind::Split) {
                chosen = Some((n, store));
                break;
            }
        }
        let (n, store) = chosen.expect("expected local leaf split on value growth before rebuild");
        let restored = store.to_index(64);
        assert_eq!(
            restored.entries.get(&key).unwrap().payload.len(),
            n as usize
        );
        let leaf_count = store
            .nodes
            .values()
            .filter(|record| matches!(record, PropertyIndexNodeRecord::Leaf { .. }))
            .count();
        assert!(leaf_count >= 2, "split should introduce another leaf");
        let ids: Vec<_> = restored.entries.keys().map(|k| k.entity_id).collect();
        assert_eq!(ids, vec![1u64, 2]);
        for (nid, record) in &store.nodes {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(
                    store.encode_node_page(record).is_ok(),
                    "leaf {nid:?} must encode after value-growth split: {record:?}"
                );
            }
        }
    }

    #[test]
    fn property_index_node_store_can_propagate_first_key_change_across_multi_level_ancestors() {
        let mut store = PropertyIndexNodeStore::new(4096);
        let leaf1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                2,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(1u8), "uid", b"a".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"b".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf1, PropertyIndexNodeId(3)),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(3u8), "uid", b"c".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let leaf3 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, leaf2, PropertyIndexNodeId(4)),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(5u8), "uid", b"e".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(6u8), "uid", b"f".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let leaf4 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, leaf3, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(7u8), "uid", b"g".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf1) {
            header.next_leaf = leaf2;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf2) {
            header.next_leaf = leaf3;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(leaf3) {
            header.next_leaf = leaf4;
        }

        let internal_left = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(3u8),
                "uid",
                b"c".to_vec(),
            )],
            children: vec![leaf1, leaf2],
        });
        let internal_right = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(7u8),
                "uid",
                b"g".to_vec(),
            )],
            children: vec![leaf3, leaf4],
        });
        let root = store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(5u8),
                "uid",
                b"e".to_vec(),
            )],
            children: vec![internal_left, internal_right],
        });

        assert!(store.remove_leaf_chain_entry(&PropertyIndexKey::node(
            NodeId::from(5u8),
            "uid",
            b"e".to_vec(),
        )));

        let restored = store.to_index(64);
        let ids: Vec<_> = restored.entries.keys().map(|key| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2, 3, 6, 7]);
        let _ = internal_right;
        let _ = root;
        match store.get(restored.header.root).expect("root internal") {
            PropertyIndexNodeRecord::Internal { keys, .. } => {
                assert_eq!(keys[0].entity_id, 6);
            }
            PropertyIndexNodeRecord::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn property_index_internal_node_record_round_trips() {
        let record = PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(2),
            keys: vec![
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
            ],
            children: vec![
                PropertyIndexNodeId(20),
                PropertyIndexNodeId(21),
                PropertyIndexNodeId(22),
            ],
        };
        let decoded = PropertyIndexNodeRecord::decode(&record.encode().expect("encode internal"))
            .expect("decode internal");
        assert_eq!(decoded, record);
    }

    #[test]
    fn property_index_snapshot_round_trips_through_bucket_region() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_index = PropertyIndex::new(64);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let mut edge_index = PropertyIndex::new(64);
        edge_index.insert(
            PropertyIndexKey::edge(9, "weight", 5_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        let snapshot = PropertyIndexSnapshot {
            node_index,
            edge_index,
        };

        write_property_index_snapshot_to_stable_memory(&mut manager, &memory, &snapshot)
            .expect("write snapshot");
        let restored = read_property_index_snapshot_from_stable_memory(&manager, &memory)
            .expect("read snapshot");

        assert_eq!(restored, snapshot);
    }

    #[test]
    fn property_index_storage_image_round_trips_through_bucket_region() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_index = PropertyIndex::new(64);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let mut edge_index = PropertyIndex::new(64);
        edge_index.insert(
            PropertyIndexKey::edge(9, "weight", 5_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index,
            },
            4096,
        )
        .unwrap();

        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");
        let restored = read_property_index_storage_image_from_stable_memory(&manager, &memory)
            .expect("read image");

        assert_eq!(restored, image);
    }

    #[test]
    fn property_index_section_readers_round_trip_through_bucket_region() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_index = PropertyIndex::new(64);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let mut edge_index = PropertyIndex::new(64);
        edge_index.insert(
            PropertyIndexKey::edge(99, "weight", 7_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index: node_index.clone(),
                edge_index: edge_index.clone(),
            },
            256,
        )
        .unwrap();

        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let header = read_property_index_region_header_from_stable_memory(&manager, &memory)
            .expect("read header");
        assert_eq!(header.version, PropertyIndexStorageImage::VERSION);

        let snapshot = read_property_index_snapshot_section_from_stable_memory(&manager, &memory)
            .expect("read snapshot section");
        assert_eq!(snapshot.node_index, node_index);
        assert_eq!(snapshot.edge_index, edge_index);

        let node_store = read_node_property_index_paged_area_from_stable_memory(&manager, &memory)
            .expect("read node paged area");
        let edge_store = read_edge_property_index_paged_area_from_stable_memory(&manager, &memory)
            .expect("read edge paged area");
        assert_eq!(node_store, image.node_store);
        assert_eq!(edge_store, image.edge_store);
    }

    #[test]
    fn property_index_direct_node_reader_reads_single_slot_leaf_record() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_index = PropertyIndex::new(64);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index: PropertyIndex::new(64),
            },
            4096,
        )
        .unwrap();
        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let record = read_node_property_index_node_record_from_stable_memory(
            &manager,
            &memory,
            PropertyIndexNodeId(1),
        )
        .expect("read node record");

        match record {
            PropertyIndexNodeRecord::Leaf { entries, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.entity_id, 1);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
    }

    #[test]
    fn property_index_direct_node_reader_reads_overflow_backed_leaf_record() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_index = PropertyIndex::new(64);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", vec![0u8; 512]),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index: PropertyIndex::new(64),
            },
            128,
        )
        .unwrap();
        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let record = read_node_property_index_node_record_from_stable_memory(
            &manager,
            &memory,
            PropertyIndexNodeId(1),
        )
        .expect("read overflow-backed node record");

        match record {
            PropertyIndexNodeRecord::Leaf { entries, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.entity_id, 1);
                assert_eq!(entries[0].0.encoded_value.len(), 512);
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf"),
        }
    }

    #[test]
    fn property_index_direct_value_scan_reads_internal_root_leaf_chain_from_stable_memory() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_store = PropertyIndexNodeStore::new(256);
        let left = node_store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let right = node_store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, left, PropertyIndexNodeId::NULL),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let _root = node_store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"bob".to_vec(),
            )],
            children: vec![left, right],
        });
        let image = PropertyIndexStorageImage {
            snapshot: PropertyIndexSnapshot::empty(64),
            node_store,
            edge_store: PropertyIndexNodeStore::new(256),
        };
        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let matches = scan_node_property_index_value_prefix_from_stable_memory(
            &manager, &memory, "uid", b"bob",
        )
        .expect("scan direct value prefix");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.entity_id, 2);
    }

    #[test]
    fn property_index_direct_property_scan_reads_property_prefix_from_stable_memory() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut node_store = PropertyIndexNodeStore::new(256);
        let left = node_store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![(
                PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
                PropertyIndexEntry::empty(),
            )],
        });
        let right = node_store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(2, left, PropertyIndexNodeId::NULL),
            entries: vec![
                (
                    PropertyIndexKey::node(NodeId::from(2u8), "uid", b"bob".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
                (
                    PropertyIndexKey::node(NodeId::from(3u8), "name", b"carol".to_vec()),
                    PropertyIndexEntry::empty(),
                ),
            ],
        });
        let _root = node_store.allocate(PropertyIndexNodeRecord::Internal {
            header: PropertyIndexNodeHeader::internal(1),
            keys: vec![PropertyIndexKey::node(
                NodeId::from(2u8),
                "uid",
                b"bob".to_vec(),
            )],
            children: vec![left, right],
        });
        let image = PropertyIndexStorageImage {
            snapshot: PropertyIndexSnapshot::empty(64),
            node_store,
            edge_store: PropertyIndexNodeStore::new(256),
        };
        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let matches =
            scan_node_property_index_property_prefix_from_stable_memory(&manager, &memory, "uid")
                .expect("scan direct property prefix");

        let ids: Vec<_> = matches.into_iter().map(|(key, _)| key.entity_id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn property_index_direct_edge_value_scan_reads_from_stable_memory() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut edge_index = PropertyIndex::new(64);
        edge_index.insert(
            PropertyIndexKey::edge(7, "weight", 5_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        edge_index.insert(
            PropertyIndexKey::edge(8, "weight", 9_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index: PropertyIndex::new(64),
                edge_index,
            },
            256,
        )
        .unwrap();
        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let matches = scan_edge_property_index_value_prefix_from_stable_memory(
            &manager,
            &memory,
            "weight",
            &5_i64.to_be_bytes(),
        )
        .expect("scan edge direct value prefix");

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.entity_id, 7);
    }

    #[test]
    fn property_index_direct_edge_property_scan_reads_property_prefix_from_stable_memory() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(RegionKind::PropertyIndex, default_property_region_chain());

        let mut edge_index = PropertyIndex::new(64);
        edge_index.insert(
            PropertyIndexKey::edge(7, "weight", 5_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        edge_index.insert(
            PropertyIndexKey::edge(8, "weight", 9_i64.to_be_bytes().to_vec()),
            PropertyIndexEntry::empty(),
        );
        edge_index.insert(
            PropertyIndexKey::edge(9, "kind", b"authored".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index: PropertyIndex::new(64),
                edge_index,
            },
            256,
        )
        .unwrap();
        write_property_index_storage_image_to_stable_memory(&mut manager, &memory, &image)
            .expect("write image");

        let matches = scan_edge_property_index_property_prefix_from_stable_memory(
            &manager, &memory, "weight",
        )
        .expect("scan edge direct property prefix");

        let ids: Vec<_> = matches.into_iter().map(|(key, _)| key.entity_id).collect();
        assert_eq!(ids, vec![7, 8]);
    }

    #[test]
    fn property_index_storage_image_v2_uses_paged_node_store_encoding() {
        let mut node_index = PropertyIndex::new(64);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let image = PropertyIndexStorageImage::try_from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index: PropertyIndex::new(64),
            },
            256,
        )
        .unwrap();

        let encoded = image.encode().expect("encode image");
        assert_eq!(encoded[4], PropertyIndexStorageImage::VERSION);
        let restored = PropertyIndexStorageImage::decode(&encoded).expect("decode image");
        assert_eq!(restored, image);
    }

    #[test]
    fn property_index_region_header_round_trips_fixed_width_encoding() {
        let header = PropertyIndexRegionHeader {
            version: 2,
            reserved: [0; 3],
            snapshot_len: 111,
            node_store_len: 222,
            edge_store_len: 333,
        };
        let decoded =
            PropertyIndexRegionHeader::decode(&header.encode()).expect("decode region header");
        assert_eq!(decoded, header);
    }

    #[test]
    fn transitional_overflow_persistence_error_display_is_stable() {
        let e = PropertyIndexError::OverflowPersistenceNotYetSupported(PropertyIndexNodeId(42));
        let s = e.to_string();
        assert!(s.contains("overflow-page persistence"), "{s}");
        assert!(s.contains('4') && s.contains('2'), "{s}");
    }

    #[test]
    fn property_index_storage_image_can_reconcile_snapshot_from_node_store() {
        let mut image = PropertyIndexStorageImage::empty(64, 4096);
        let mut index = PropertyIndex::new(64);
        index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        image.node_store = PropertyIndexNodeStore::try_from_index(&index, 4096).unwrap();

        image
            .try_reconcile(64, 4096)
            .expect("reconcile must succeed for valid fixture");

        assert_eq!(image.snapshot.node_index.entries, index.entries);
        assert_eq!(
            image.snapshot.node_index.header.entry_count,
            index.header.entry_count
        );
        assert_eq!(
            image.snapshot.node_index.header.root,
            PropertyIndexNodeId(1)
        );
        assert_eq!(
            image.snapshot.node_index.header.first_leaf,
            PropertyIndexNodeId(1)
        );
        assert_eq!(
            image.snapshot.node_index.header.last_leaf,
            PropertyIndexNodeId(1)
        );
    }

    /// Compact-style image: empty PIDX snapshot bytes, authoritative node store — decode preserves
    /// that shape; `try_normalized` restores logical indices (same contract as facade hydration).
    #[test]
    fn property_index_storage_image_empty_snapshot_with_stores_round_trips_through_encode_normalize()
     {
        let bf = 64u16;
        let page = 4096u32;
        let mut node_index = PropertyIndex::new(bf);
        node_index.insert(
            PropertyIndexKey::node(NodeId::from(7u8), "uid", b"grace".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let mut edge_index = PropertyIndex::new(bf);
        edge_index.insert(
            PropertyIndexKey::edge(404, "kind", b"authored".to_vec()),
            PropertyIndexEntry::empty(),
        );
        let node_store = PropertyIndexNodeStore::try_from_index(&node_index, page).unwrap();
        let edge_store = PropertyIndexNodeStore::try_from_index(&edge_index, page).unwrap();

        let compact = PropertyIndexStorageImage {
            snapshot: PropertyIndexSnapshot::empty(bf),
            node_store,
            edge_store,
        };
        assert_eq!(compact.snapshot.node_index.header.entry_count, 0);
        assert_eq!(compact.snapshot.edge_index.header.entry_count, 0);
        assert!(!compact.node_store.nodes.is_empty());
        assert!(!compact.edge_store.nodes.is_empty());

        let bytes = compact.encode().expect("encode compact image");
        let decoded = PropertyIndexStorageImage::decode(&bytes).expect("decode");
        assert_eq!(decoded.snapshot.node_index.header.entry_count, 0);
        assert_eq!(decoded.snapshot.edge_index.header.entry_count, 0);

        let restored = decoded
            .try_normalized(bf, page)
            .expect("normalize must succeed for valid fixture");
        assert_eq!(restored.snapshot.node_index.entries, node_index.entries);
        assert_eq!(restored.snapshot.edge_index.entries, edge_index.entries);
    }

    /// Page-aware chunking and `repartition_three_leaf_window_from_merged_entries` are covered
    /// directly here. Pairwise insert/remove redistribution uses the same single-page check as
    /// [`PropertyIndexNodeStore::encode_node_page`] via [`PropertyIndexNodeStore::find_leaf_redistribution_split`].
    #[test]
    fn partition_entries_into_leaf_chunks_single_chunk_when_page_large() {
        let store = PropertyIndexNodeStore::new(4096);
        let entries: Vec<_> = (1u8..=12)
            .map(|id| {
                (
                    PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; 8]),
                    PropertyIndexEntry::empty(),
                )
            })
            .collect();
        let chunks = store
            .partition_entries_into_leaf_chunks(entries.clone())
            .unwrap();
        assert_eq!(
            chunks.len(),
            1,
            "expected one page-sized chunk, got {}",
            chunks.len()
        );
        assert_eq!(chunks[0].len(), 12);
    }

    #[test]
    fn partition_entries_into_leaf_chunks_many_chunks_when_page_tight() {
        let store = PropertyIndexNodeStore::new(220);
        let entries: Vec<_> = (1u8..=20)
            .map(|id| {
                (
                    PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; 48]),
                    PropertyIndexEntry::empty(),
                )
            })
            .collect();
        let chunks = store
            .partition_entries_into_leaf_chunks(entries.clone())
            .unwrap();
        assert!(
            chunks.len() >= 5,
            "expected at least 5 single-page chunks, got {}",
            chunks.len()
        );
    }

    /// One entry may exceed a single primary page; partition still yields one chunk and it encodes
    /// via overflow pages (see [`PropertyIndexNodeStore::encode_node_pages`]).
    #[test]
    fn partition_entries_into_leaf_chunks_allows_overflow_singleton() {
        let page = 256u32;
        let store = PropertyIndexNodeStore::new(page);
        let key = PropertyIndexKey::node(NodeId::from(1u8), "uid", vec![0x11; 4]);
        let entry = PropertyIndexEntry {
            payload: vec![0x55u8; 512],
        };
        let chunks = store
            .partition_entries_into_leaf_chunks(vec![(key, entry)])
            .unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 1);
        let leaf = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId::NULL,
            ),
            entries: chunks[0].clone(),
        };
        assert!(
            store.encode_node_page(&leaf).is_err(),
            "oversized singleton should not fit the primary page alone"
        );
        let pages = store.encode_node_pages(&leaf).expect("paged encoding");
        assert!(
            pages.len() > 1,
            "expected overflow pages, got {}",
            pages.len()
        );
    }

    /// `repartition_three_leaf_window_from_merged_entries` with one merged chunk collapses to `l0`.
    #[test]
    fn three_leaf_repartition_collapses_to_one_leaf_when_partition_yields_single_chunk() {
        let page = 512u32;
        let mut store = PropertyIndexNodeStore::new(page);
        let mk = |id: u8| PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; 8]);
        let e1 = (mk(1), PropertyIndexEntry::empty());
        let e2 = (mk(2), PropertyIndexEntry::empty());
        let e3 = (mk(3), PropertyIndexEntry::empty());
        let old_firsts = [Some(mk(1)), Some(mk(2)), Some(mk(3))];

        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                1,
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: vec![e1.clone()],
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, l0, PropertyIndexNodeId::NULL),
            entries: vec![e2.clone()],
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(1, l1, PropertyIndexNodeId::NULL),
            entries: vec![e3.clone()],
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }

        let merged = vec![e1.0.clone(), e2.0.clone(), e3.0.clone()]
            .into_iter()
            .zip(std::iter::repeat(PropertyIndexEntry::empty()))
            .collect::<Vec<_>>();
        assert_eq!(
            store
                .partition_entries_into_leaf_chunks(merged.clone())
                .unwrap()
                .len(),
            1
        );

        assert!(store.repartition_three_leaf_window_from_merged_entries(
            crate::property_index::node_store::ThreeLeafRepartitionInput {
                l0,
                l1,
                l2,
                prev0: PropertyIndexNodeId::NULL,
                next2: PropertyIndexNodeId::NULL,
                old_firsts,
                merged,
            },
        ));

        assert_eq!(
            store
                .nodes
                .values()
                .filter(|r| matches!(r, PropertyIndexNodeRecord::Leaf { .. }))
                .count(),
            1
        );
        match store.get(l0).expect("l0 leaf") {
            PropertyIndexNodeRecord::Leaf { entries, .. } => {
                assert_eq!(entries.len(), 3);
                assert!(store.encode_node_page(store.get(l0).expect("l0")).is_ok());
            }
            PropertyIndexNodeRecord::Internal { .. } => panic!("expected leaf at l0"),
        }
    }

    /// Five or more chunks allocate extra leaf ids past the original window.
    #[test]
    fn three_leaf_repartition_expands_chain_when_partition_yields_five_or_more_chunks() {
        let page = 220u32;
        let mut store = PropertyIndexNodeStore::new(page);
        let mk = |id: u8| PropertyIndexKey::node(NodeId::from(id), "uid", vec![id; 48]);
        let merged: Vec<_> = (1u8..=15)
            .map(|id| (mk(id), PropertyIndexEntry::empty()))
            .collect();
        let chunk_count = store
            .partition_entries_into_leaf_chunks(merged.clone())
            .unwrap()
            .len();
        assert!(
            chunk_count >= 5,
            "fixture needs >= 5 chunks, got {chunk_count}"
        );

        let old_firsts = [Some(mk(1)), Some(mk(5)), Some(mk(9))];
        let mut entries0 = merged[0..5].to_vec();
        let mut entries1 = merged[5..10].to_vec();
        let mut entries2 = merged[10..15].to_vec();
        let l0 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                u16::try_from(entries0.len()).unwrap_or(u16::MAX),
                PropertyIndexNodeId::NULL,
                PropertyIndexNodeId(2),
            ),
            entries: std::mem::take(&mut entries0),
        });
        let l1 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                u16::try_from(entries1.len()).unwrap_or(u16::MAX),
                l0,
                PropertyIndexNodeId::NULL,
            ),
            entries: std::mem::take(&mut entries1),
        });
        let l2 = store.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                u16::try_from(entries2.len()).unwrap_or(u16::MAX),
                l1,
                PropertyIndexNodeId::NULL,
            ),
            entries: std::mem::take(&mut entries2),
        });
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l0) {
            header.next_leaf = l1;
        }
        if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(l1) {
            header.next_leaf = l2;
        }

        assert!(store.repartition_three_leaf_window_from_merged_entries(
            crate::property_index::node_store::ThreeLeafRepartitionInput {
                l0,
                l1,
                l2,
                prev0: PropertyIndexNodeId::NULL,
                next2: PropertyIndexNodeId::NULL,
                old_firsts,
                merged,
            },
        ));

        let leaf_count = store
            .nodes
            .values()
            .filter(|r| matches!(r, PropertyIndexNodeRecord::Leaf { .. }))
            .count();
        assert!(
            leaf_count >= chunk_count,
            "expected at least {chunk_count} leaves, got {leaf_count}",
        );
        assert!(leaf_count >= 5);
        for record in store.nodes.values() {
            if let PropertyIndexNodeRecord::Leaf { .. } = record {
                assert!(store.encode_node_page(record).is_ok());
            }
        }
    }
}
