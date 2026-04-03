use std::collections::{BTreeMap, BTreeSet};

use super::{
    PropertyIndex, PropertyIndexAllocatorHeader, PropertyIndexEntityKind, PropertyIndexEntry,
    PropertyIndexError, PropertyIndexKey, PropertyIndexLeafChainShapeError,
    PropertyIndexNodeHeader, PropertyIndexNodeId, PropertyIndexNodeRecord,
};

/// In-memory allocator-side image for persisted property-index nodes.
///
/// This is the metadata-first stepping stone between the current whole-index
/// snapshot and a future bucket-backed node/page allocator. It keeps node ids,
/// allocator header state, and encoded node records separate from the logical
/// `PropertyIndex`.
///
/// **Incremental hot paths** (local update, pairwise redistribution, most leaf splits) require each
/// touched leaf to encode under [`PropertyIndexNodeStore::encode_node_page`] (one primary page).
///
/// **Paged persistence** uses [`PropertyIndexNodeStore::encode_node_pages`] so a **singleton** leaf
/// whose key/value exceeds one primary page may still round-trip through the paged area (overflow
/// slots). [`PropertyIndexNodeStore::partition_entries_into_leaf_chunks`] guarantees any chunk with
/// **two or more** entries fits one primary page; a trailing **one-entry** chunk may require overflow.
#[derive(Clone, Debug)]
pub struct PropertyIndexNodeStore {
    pub allocator: PropertyIndexAllocatorHeader,
    pub free_node_ids: Vec<PropertyIndexNodeId>,
    pub nodes: BTreeMap<PropertyIndexNodeId, PropertyIndexNodeRecord>,
    /// When true, this store's paged area may differ from stable memory and must be encoded on the
    /// next PIDX flush (unless the facade forces a full image write). Cleared after a successful
    /// compact PIDX flush. Not compared by [`PartialEq`].
    pub pidx_side_must_flush: bool,
}

impl PartialEq for PropertyIndexNodeStore {
    fn eq(&self, other: &Self) -> bool {
        self.allocator == other.allocator
            && self.free_node_ids == other.free_node_ids
            && self.nodes == other.nodes
    }
}

impl Eq for PropertyIndexNodeStore {}

/// Difference summary between two persisted node-store states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyIndexNodeStoreDelta {
    /// Node ids whose persisted record changed or was newly allocated/freed.
    pub touched_node_ids: Vec<PropertyIndexNodeId>,
    /// Newly allocated node ids that were absent in the previous state.
    pub allocated_node_ids: Vec<PropertyIndexNodeId>,
    /// Node ids that were present in the previous state and were freed.
    pub freed_node_ids: Vec<PropertyIndexNodeId>,
}

/// Coarse-grained incremental mutation path taken by the persisted node store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyIndexNodeStoreMutationKind {
    /// The target leaf was updated in place without changing the tree shape.
    LocalUpdate,
    /// Two adjacent leaves were redistributed without allocating or freeing nodes.
    Redistribute,
    /// Three consecutive leaves were merged and repartitioned into page-sized chunks (may allocate,
    /// free, or rebuild internal levels while keeping the update local to that window).
    ThreeLeafRepack,
    /// A leaf split introduced one or more newly allocated nodes.
    Split,
    /// Two adjacent leaves were merged into one surviving leaf.
    Merge,
    /// One leaf became empty and was collapsed out of the leaf chain.
    Collapse,
    /// Incremental repair could not handle the shape and the caller rebuilt the node store.
    Rebuild,
}
/// Three consecutive leaves `l0 → l1 → l2` with outer chain hooks (`prev0`, `next2`), after
/// validating `prev` / `next` links between them.
struct OrderedThreeLeafWindow {
    l0: PropertyIndexNodeId,
    l1: PropertyIndexNodeId,
    l2: PropertyIndexNodeId,
    prev0: PropertyIndexNodeId,
    next2: PropertyIndexNodeId,
    e0: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
    e1: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
    e2: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
}

pub(crate) struct ThreeLeafRepartitionInput {
    pub(crate) l0: PropertyIndexNodeId,
    pub(crate) l1: PropertyIndexNodeId,
    pub(crate) l2: PropertyIndexNodeId,
    pub(crate) prev0: PropertyIndexNodeId,
    pub(crate) next2: PropertyIndexNodeId,
    pub(crate) old_firsts: [Option<PropertyIndexKey>; 3],
    pub(crate) merged: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
}

/// Parsed fixed header + layout bounds for a paged property-index area (incremental flush).
struct PagedAreaParsedPrefix {
    version: u8,
    allocator: PropertyIndexAllocatorHeader,
    free_node_ids: Vec<PropertyIndexNodeId>,
    page_count: usize,
    overflow_page_count: usize,
    pages_start: usize,
    page_size: usize,
}

impl OrderedThreeLeafWindow {
    fn old_firsts(&self) -> [Option<PropertyIndexKey>; 3] {
        [
            self.e0.first().map(|(k, _)| k.clone()),
            self.e1.first().map(|(k, _)| k.clone()),
            self.e2.first().map(|(k, _)| k.clone()),
        ]
    }

    fn merged_entries(&self) -> Vec<(PropertyIndexKey, PropertyIndexEntry)> {
        let mut merged = self.e0.clone();
        merged.extend(self.e1.iter().cloned());
        merged.extend(self.e2.iter().cloned());
        merged
    }
}

impl PropertyIndexNodeStore {
    /// Snapshot magic for persisted node-store images.
    pub const MAGIC: [u8; 4] = *b"PINS";

    /// Current node-store image layout version.
    pub const VERSION: u8 = 1;

    /// Magic stored at the beginning of one fixed-size node page.
    pub const NODE_PAGE_MAGIC: [u8; 4] = *b"PINP";

    /// Current fixed-size node-page layout version.
    pub const NODE_PAGE_VERSION: u8 = 1;

    /// Fixed-width header size for one persisted node page.
    pub const NODE_PAGE_HEADER_LEN: usize = 4 + 1 + 4 + 8;

    /// Magic stored at the beginning of one overflow page.
    pub const NODE_OVERFLOW_PAGE_MAGIC: [u8; 4] = *b"PINO";

    /// Current overflow-page layout version.
    pub const NODE_OVERFLOW_PAGE_VERSION: u8 = 1;

    /// Fixed-width header size for one overflow page.
    pub const NODE_OVERFLOW_PAGE_HEADER_LEN: usize = 4 + 1 + 8;

    /// Magic stored at the beginning of one paged node-store area.
    pub const PAGED_AREA_MAGIC: [u8; 4] = *b"PINA";

    /// Current paged node-store area layout version.
    pub const PAGED_AREA_VERSION: u8 = 2;

    /// Fixed prefix length of a paged-area before the free-list entries.
    pub const PAGED_AREA_FIXED_HEADER_LEN: usize =
        4 + 1 + PropertyIndexAllocatorHeader::ENCODED_LEN + 4 + 8 + 8;

    /// Creates one empty node store.
    pub fn new(page_size_bytes: u32) -> Self {
        Self {
            allocator: PropertyIndexAllocatorHeader::empty(page_size_bytes),
            free_node_ids: Vec::new(),
            nodes: BTreeMap::new(),
            pidx_side_must_flush: true,
        }
    }

    /// Allocates one node id and stores the given node record.
    pub fn allocate(&mut self, node: PropertyIndexNodeRecord) -> PropertyIndexNodeId {
        let id = self.free_node_ids.pop().unwrap_or_else(|| {
            let id = PropertyIndexNodeId(self.allocator.next_node_id);
            self.allocator.next_node_id += 1;
            id
        });
        self.nodes.insert(id, node);
        self.allocator.free_list_head = self
            .free_node_ids
            .last()
            .copied()
            .unwrap_or(PropertyIndexNodeId::NULL);
        self.pidx_side_must_flush = true;
        id
    }

    /// Releases one node id back into the free list.
    pub fn free(&mut self, node_id: PropertyIndexNodeId) -> Option<PropertyIndexNodeRecord> {
        let removed = self.nodes.remove(&node_id)?;
        self.free_node_ids.push(node_id);
        self.allocator.free_list_head = node_id;
        self.pidx_side_must_flush = true;
        Some(removed)
    }

    /// Returns one persisted node record by id.
    pub fn get(&self, node_id: PropertyIndexNodeId) -> Option<&PropertyIndexNodeRecord> {
        self.nodes.get(&node_id)
    }

    /// Returns mutable access to one persisted node record by id.
    pub fn get_mut(
        &mut self,
        node_id: PropertyIndexNodeId,
    ) -> Option<&mut PropertyIndexNodeRecord> {
        self.nodes.get_mut(&node_id)
    }

    /// Returns one before/after delta summary for this node store.
    pub fn diff_against(&self, previous: &Self) -> PropertyIndexNodeStoreDelta {
        let mut touched = BTreeSet::new();
        let mut allocated = Vec::new();
        let mut freed = Vec::new();

        for (node_id, record) in &self.nodes {
            match previous.nodes.get(node_id) {
                Some(old_record) if old_record == record => {}
                Some(_) => {
                    touched.insert(*node_id);
                }
                None => {
                    touched.insert(*node_id);
                    allocated.push(*node_id);
                }
            }
        }

        for node_id in previous.nodes.keys() {
            if !self.nodes.contains_key(node_id) {
                touched.insert(*node_id);
                freed.push(*node_id);
            }
        }

        PropertyIndexNodeStoreDelta {
            touched_node_ids: touched.into_iter().collect(),
            allocated_node_ids: allocated,
            freed_node_ids: freed,
        }
    }

    /// Encodes one whole node-store image.
    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let mut out = Vec::new();
        out.extend_from_slice(&Self::MAGIC);
        out.push(Self::VERSION);
        out.extend_from_slice(&self.allocator.encode());
        out.extend_from_slice(
            &u32::try_from(self.free_node_ids.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(self.nodes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        for free_id in &self.free_node_ids {
            out.extend_from_slice(&free_id.0.to_le_bytes());
        }
        for (node_id, node) in &self.nodes {
            let node_bytes = node.encode()?;
            out.extend_from_slice(&node_id.0.to_le_bytes());
            out.extend_from_slice(
                &u32::try_from(node_bytes.len())
                    .map_err(|_| PropertyIndexError::LengthOverflow)?
                    .to_le_bytes(),
            );
            out.extend_from_slice(&node_bytes);
        }
        Ok(out)
    }

    /// Decodes one whole node-store image.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        let min_len = 4 + 1 + PropertyIndexAllocatorHeader::ENCODED_LEN + 4 + 4;
        if bytes.len() < min_len {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        if bytes[..4] != Self::MAGIC {
            return Err(PropertyIndexError::InvalidMagic(bytes[..4].to_vec()));
        }
        if bytes[4] != Self::VERSION {
            return Err(PropertyIndexError::UnsupportedVersion(bytes[4]));
        }
        let allocator_start = 5;
        let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
        let allocator =
            PropertyIndexAllocatorHeader::decode(&bytes[allocator_start..allocator_end])?;
        let mut free_count = [0u8; 4];
        free_count.copy_from_slice(&bytes[allocator_end..allocator_end + 4]);
        let free_count = u32::from_le_bytes(free_count) as usize;
        let mut node_count = [0u8; 4];
        node_count.copy_from_slice(&bytes[allocator_end + 4..allocator_end + 8]);
        let node_count = u32::from_le_bytes(node_count) as usize;
        let mut offset = allocator_end + 8;

        let mut free_node_ids = Vec::with_capacity(free_count);
        for _ in 0..free_count {
            if bytes.len().saturating_sub(offset) < 8 {
                return Err(PropertyIndexError::RecordTooShort(
                    bytes.len().saturating_sub(offset),
                ));
            }
            let mut free_id = [0u8; 8];
            free_id.copy_from_slice(&bytes[offset..offset + 8]);
            free_node_ids.push(PropertyIndexNodeId(u64::from_le_bytes(free_id)));
            offset += 8;
        }

        let mut nodes = BTreeMap::new();
        for _ in 0..node_count {
            if bytes.len().saturating_sub(offset) < 12 {
                return Err(PropertyIndexError::RecordTooShort(
                    bytes.len().saturating_sub(offset),
                ));
            }
            let mut node_id = [0u8; 8];
            node_id.copy_from_slice(&bytes[offset..offset + 8]);
            let node_id = PropertyIndexNodeId(u64::from_le_bytes(node_id));
            let mut node_len = [0u8; 4];
            node_len.copy_from_slice(&bytes[offset + 8..offset + 12]);
            let node_len = u32::from_le_bytes(node_len) as usize;
            offset += 12;
            let node_end = offset
                .checked_add(node_len)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            if node_end > bytes.len() {
                return Err(PropertyIndexError::RecordLengthMismatch {
                    expected: node_end,
                    actual: bytes.len(),
                });
            }
            let node = PropertyIndexNodeRecord::decode(&bytes[offset..node_end])?;
            nodes.insert(node_id, node);
            offset = node_end;
        }

        Ok(Self {
            allocator,
            free_node_ids,
            nodes,
            pidx_side_must_flush: true,
        })
    }

    /// Builds one minimal persisted node-store image from the current logical index.
    ///
    /// The current phase builds one page-aware leaf chain and then stacks
    /// internal routing layers using the logical branching factor.
    pub fn try_from_index(
        index: &PropertyIndex,
        page_size_bytes: u32,
    ) -> Result<Self, PropertyIndexError> {
        let mut store = Self::new(page_size_bytes);
        if index.entries.is_empty() {
            return Ok(store);
        }
        let entries: Vec<_> = index
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        let leaf_chunks = store.partition_entries_into_leaf_chunks(entries)?;
        let mut leaf_ids = Vec::with_capacity(leaf_chunks.len());
        for chunk in leaf_chunks {
            let prev_leaf = leaf_ids
                .last()
                .copied()
                .unwrap_or(PropertyIndexNodeId::NULL);
            let leaf_id = store.allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(chunk.len()).unwrap_or(u16::MAX),
                    prev_leaf,
                    PropertyIndexNodeId::NULL,
                ),
                entries: chunk,
            });
            if let Some(previous_leaf) = leaf_ids.last().copied()
                && let Some(PropertyIndexNodeRecord::Leaf { header, .. }) =
                    store.get_mut(previous_leaf)
            {
                header.next_leaf = leaf_id;
            }
            leaf_ids.push(leaf_id);
        }
        let fanout = usize::from(index.header.branching_factor.max(2));
        let _ = store.build_internal_levels_from_leaf_chain(&leaf_ids, fanout);
        Ok(store)
    }

    /// Inserts or replaces one entry in-place when the node store is still in the single-leaf phase.
    ///
    /// Returns `true` when the persisted node store was updated incrementally.
    /// Returns `false` when the caller should fall back to rebuilding from the
    /// logical index.
    pub fn upsert_single_leaf_entry(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> bool {
        match self.single_leaf_id() {
            None => {
                let _ = self.allocate(PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        1,
                        PropertyIndexNodeId::NULL,
                        PropertyIndexNodeId::NULL,
                    ),
                    entries: vec![(key, entry)],
                });
                true
            }
            Some(leaf_id) => {
                let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.get_mut(leaf_id)
                else {
                    return false;
                };
                match entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
                    Ok(index) => entries[index] = (key, entry),
                    Err(index) => entries.insert(index, (key, entry)),
                }
                header.entry_count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
                header.prev_leaf = PropertyIndexNodeId::NULL;
                header.next_leaf = PropertyIndexNodeId::NULL;
                true
            }
        }
    }

    /// Inserts or replaces one entry in-place when the node store is a leaf chain without internal nodes.
    ///
    /// Returns `true` when the persisted node store was updated incrementally.
    /// Returns `false` when the caller should fall back to rebuilding from the
    /// logical index.
    pub fn upsert_leaf_chain_entry(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> bool {
        self.upsert_leaf_chain_entry_with_kind(key, entry).is_some()
    }

    /// Inserts or replaces one entry and reports the incremental node-store path used.
    pub fn upsert_leaf_chain_entry_with_kind(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        let out = self.upsert_leaf_chain_entry_with_kind_inner(key, entry);
        if out.is_some() {
            self.pidx_side_must_flush = true;
        }
        out
    }

    fn upsert_leaf_chain_entry_with_kind_inner(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        if let Some(leaf_id) = self.single_leaf_id() {
            let _ = leaf_id;
            return self
                .upsert_single_leaf_entry(key, entry)
                .then_some(PropertyIndexNodeStoreMutationKind::LocalUpdate);
        }
        if self.try_upsert_entry_locally(key.clone(), entry.clone()) {
            return Some(PropertyIndexNodeStoreMutationKind::LocalUpdate);
        }
        if let Some(kind) =
            self.try_upsert_entry_with_leaf_redistribution(key.clone(), entry.clone())
        {
            return Some(kind);
        }
        if self.try_upsert_entry_with_leaf_split(key.clone(), entry.clone()) {
            return Some(PropertyIndexNodeStoreMutationKind::Split);
        }
        let (leaf_ids, internal_ids, fanout) = self.incremental_leaf_chain_shape()?;
        let target_leaf_len = self.max_leaf_entry_count(&leaf_ids).max(1);
        let mut entries = self.collect_leaf_chain_entries(&leaf_ids);
        match entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
            Ok(index) => entries[index] = (key, entry),
            Err(index) => entries.insert(index, (key, entry)),
        }
        self.rewrite_leaf_chain_entries(leaf_ids, internal_ids, fanout, entries, target_leaf_len)
            .then_some(PropertyIndexNodeStoreMutationKind::Rebuild)
    }

    fn try_upsert_entry_with_leaf_redistribution(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        let (path, leaf_id) = self.find_path_to_leaf_for_key(&key)?;
        let (leaf_entries, prev_leaf, next_leaf) = match self.get(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                (entries.clone(), header.prev_leaf, header.next_leaf)
            }
            Some(PropertyIndexNodeRecord::Internal { .. }) | None => return None,
        };
        if leaf_entries.is_empty() {
            return None;
        }

        if !next_leaf.is_null()
            && self.try_redistribute_insert_between_leaves(
                leaf_id,
                next_leaf,
                prev_leaf,
                path.as_slice(),
                key.clone(),
                entry.clone(),
            )
        {
            return Some(PropertyIndexNodeStoreMutationKind::Redistribute);
        }

        if !prev_leaf.is_null() {
            let prev_first_key = self.first_key_for_subtree(prev_leaf);
            let prev_path = prev_first_key
                .as_ref()
                .and_then(|first_key| self.find_path_to_leaf_for_key(first_key))
                .map(|(path, _)| path);
            if let Some(prev_path) = prev_path {
                let prev_prev_leaf = match self.get(prev_leaf) {
                    Some(PropertyIndexNodeRecord::Leaf { header, .. }) => header.prev_leaf,
                    Some(PropertyIndexNodeRecord::Internal { .. }) | None => return None,
                };
                if self.try_redistribute_insert_between_leaves(
                    prev_leaf,
                    leaf_id,
                    prev_prev_leaf,
                    prev_path.as_slice(),
                    key.clone(),
                    entry.clone(),
                ) {
                    return Some(PropertyIndexNodeStoreMutationKind::Redistribute);
                }
            }
        }

        self.try_upsert_three_leaf_redistribute(leaf_id, key, entry)
            .then_some(PropertyIndexNodeStoreMutationKind::ThreeLeafRepack)
    }

    /// Shared tail for three-leaf windows: partition `merged` into single-page chunks and apply
    /// them to consecutive leaves starting at `(l0, l1, l2)` with chain `prev0 — … — next2`.
    ///
    /// One chunk collapses the window to a single leaf (frees `l1` and `l2`). Five or more
    /// chunks allocate additional leaf ids, link `l0 … l_{n-1} — next2`, and rebuild internals.
    pub(crate) fn repartition_three_leaf_window_from_merged_entries(
        &mut self,
        input: ThreeLeafRepartitionInput,
    ) -> bool {
        let ThreeLeafRepartitionInput {
            l0,
            l1,
            l2,
            prev0,
            next2,
            old_firsts,
            merged,
        } = input;
        let Ok(chunks) = self.partition_entries_into_leaf_chunks(merged) else {
            return false;
        };
        let chunk_count = chunks.len();
        if chunk_count == 0 {
            return false;
        }

        if chunk_count == 3 {
            let Some(path0) = old_firsts[0]
                .as_ref()
                .and_then(|k| self.find_path_to_leaf_for_key(k).map(|(p, _)| p))
            else {
                return false;
            };
            let Some(path1) = old_firsts[1]
                .as_ref()
                .and_then(|k| self.find_path_to_leaf_for_key(k).map(|(p, _)| p))
            else {
                return false;
            };
            let Some(path2) = old_firsts[2]
                .as_ref()
                .and_then(|k| self.find_path_to_leaf_for_key(k).map(|(p, _)| p))
            else {
                return false;
            };
            let paths = [path0, path1, path2];

            let c0 = chunks[0].clone();
            let c1 = chunks[1].clone();
            let c2 = chunks[2].clone();
            let nf0 = c0.first().map(|(k, _)| k.clone());
            let nf1 = c1.first().map(|(k, _)| k.clone());
            let nf2 = c2.first().map(|(k, _)| k.clone());

            self.nodes.insert(
                l0,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c0.len()).unwrap_or(u16::MAX),
                        prev0,
                        l1,
                    ),
                    entries: c0,
                },
            );
            self.nodes.insert(
                l1,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c1.len()).unwrap_or(u16::MAX),
                        l0,
                        l2,
                    ),
                    entries: c1,
                },
            );
            self.nodes.insert(
                l2,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c2.len()).unwrap_or(u16::MAX),
                        l1,
                        next2,
                    ),
                    entries: c2,
                },
            );
            if !next2.is_null() {
                let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next2) else {
                    return false;
                };
                header.prev_leaf = l2;
            }

            if old_firsts[0] != nf0
                && let Some(nf) = nf0
            {
                self.propagate_first_key_change(&paths[0], nf);
            }
            if old_firsts[1] != nf1
                && let Some(nf) = nf1
            {
                self.propagate_first_key_change(&paths[1], nf);
            }
            if old_firsts[2] != nf2
                && let Some(nf) = nf2
            {
                self.propagate_first_key_change(&paths[2], nf);
            }
            return true;
        }

        let Some((leaf_ids_full, internal_ids, fanout)) = self.incremental_leaf_chain_shape()
        else {
            return false;
        };
        let pos = match leaf_ids_full.iter().position(|&id| id == l0) {
            Some(p)
                if leaf_ids_full.get(p + 1) == Some(&l1)
                    && leaf_ids_full.get(p + 2) == Some(&l2) =>
            {
                p
            }
            _ => return false,
        };

        if chunk_count == 1 {
            let c0 = chunks[0].clone();
            self.nodes.insert(
                l0,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c0.len()).unwrap_or(u16::MAX),
                        prev0,
                        next2,
                    ),
                    entries: c0,
                },
            );
            if !next2.is_null() {
                let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next2) else {
                    return false;
                };
                header.prev_leaf = l0;
            }
            let mut leaf_ids_updated: Vec<_> = leaf_ids_full[..pos].to_vec();
            leaf_ids_updated.push(l0);
            leaf_ids_updated.extend_from_slice(&leaf_ids_full[pos + 3..]);
            let _ = self.free(l1);
            let _ = self.free(l2);
            self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids_updated, fanout);
            return true;
        }

        if chunk_count == 2 {
            let Some(path0) = old_firsts[0]
                .as_ref()
                .and_then(|k| self.find_path_to_leaf_for_key(k).map(|(p, _)| p))
            else {
                return false;
            };
            let Some(path1) = old_firsts[1]
                .as_ref()
                .and_then(|k| self.find_path_to_leaf_for_key(k).map(|(p, _)| p))
            else {
                return false;
            };
            let Some(path2) = old_firsts[2]
                .as_ref()
                .and_then(|k| self.find_path_to_leaf_for_key(k).map(|(p, _)| p))
            else {
                return false;
            };

            let c0 = chunks[0].clone();
            let c1 = chunks[1].clone();
            let nf0 = c0.first().map(|(k, _)| k.clone());
            let nf1 = c1.first().map(|(k, _)| k.clone());

            self.nodes.insert(
                l0,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c0.len()).unwrap_or(u16::MAX),
                        prev0,
                        l1,
                    ),
                    entries: c0,
                },
            );
            self.nodes.insert(
                l1,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c1.len()).unwrap_or(u16::MAX),
                        l0,
                        next2,
                    ),
                    entries: c1,
                },
            );
            if !next2.is_null() {
                let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next2) else {
                    return false;
                };
                header.prev_leaf = l1;
            }

            let mut leaf_ids_updated: Vec<_> = leaf_ids_full[..pos].to_vec();
            leaf_ids_updated.push(l0);
            leaf_ids_updated.push(l1);
            leaf_ids_updated.extend_from_slice(&leaf_ids_full[pos + 3..]);

            let _ = self.free(l2);
            if !self.try_remove_child_via_ancestor_compaction(&path2, l2) {
                self.rebuild_internal_levels_over_leaf_chain(
                    internal_ids,
                    &leaf_ids_updated,
                    fanout,
                );
            } else {
                if old_firsts[0] != nf0
                    && let Some(nf) = nf0
                {
                    self.propagate_first_key_change(&path0, nf);
                }
                if old_firsts[1] != nf1
                    && let Some(nf) = nf1
                {
                    self.propagate_first_key_change(&path1, nf);
                }
            }
            return true;
        }

        if chunk_count == 4 {
            let l3 = self.allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    0,
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: Vec::new(),
            });
            let c0 = chunks[0].clone();
            let c1 = chunks[1].clone();
            let c2 = chunks[2].clone();
            let c3 = chunks[3].clone();
            self.nodes.insert(
                l0,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c0.len()).unwrap_or(u16::MAX),
                        prev0,
                        l1,
                    ),
                    entries: c0,
                },
            );
            self.nodes.insert(
                l1,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c1.len()).unwrap_or(u16::MAX),
                        l0,
                        l2,
                    ),
                    entries: c1,
                },
            );
            self.nodes.insert(
                l2,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c2.len()).unwrap_or(u16::MAX),
                        l1,
                        l3,
                    ),
                    entries: c2,
                },
            );
            self.nodes.insert(
                l3,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(c3.len()).unwrap_or(u16::MAX),
                        l2,
                        next2,
                    ),
                    entries: c3,
                },
            );
            if !next2.is_null() {
                let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next2) else {
                    return false;
                };
                header.prev_leaf = l3;
            }

            let mut leaf_ids_updated: Vec<_> = leaf_ids_full[..pos].to_vec();
            leaf_ids_updated.extend([l0, l1, l2, l3]);
            leaf_ids_updated.extend_from_slice(&leaf_ids_full[pos + 3..]);

            self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids_updated, fanout);
            return true;
        }

        if chunk_count >= 5 {
            let n = chunk_count;
            let mut chain = vec![l0, l1, l2];
            while chain.len() < n {
                chain.push(self.allocate(PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        0,
                        PropertyIndexNodeId::NULL,
                        PropertyIndexNodeId::NULL,
                    ),
                    entries: Vec::new(),
                }));
            }
            for i in 0..n {
                let prev_id = if i == 0 { prev0 } else { chain[i - 1] };
                let next_id = if i + 1 < n { chain[i + 1] } else { next2 };
                let chunk_entries = chunks[i].clone();
                self.nodes.insert(
                    chain[i],
                    PropertyIndexNodeRecord::Leaf {
                        header: PropertyIndexNodeHeader::leaf(
                            u16::try_from(chunk_entries.len()).unwrap_or(u16::MAX),
                            prev_id,
                            next_id,
                        ),
                        entries: chunk_entries,
                    },
                );
            }
            if !next2.is_null() {
                let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next2) else {
                    return false;
                };
                header.prev_leaf = chain[n - 1];
            }
            let mut leaf_ids_updated: Vec<_> = leaf_ids_full[..pos].to_vec();
            leaf_ids_updated.extend(chain);
            leaf_ids_updated.extend_from_slice(&leaf_ids_full[pos + 3..]);
            self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids_updated, fanout);
            return true;
        }

        false
    }

    /// When the target leaf is not the leftmost of a triple, `try_upsert_three_leaf_redistribute`
    /// / `try_remove_three_leaf_redistribute_forward_window` must still merge **three** consecutive
    /// leaves. Walk backward until `leaf` has two non-null `next_leaf` hops (so `leaf`, `next`,
    /// `next.next` are all valid leaf ids); that leaf is the window's `l0`.
    fn three_leaf_forward_window_start(
        &self,
        mut leaf: PropertyIndexNodeId,
    ) -> Option<PropertyIndexNodeId> {
        const MAX_STEPS: usize = 64;
        for _ in 0..MAX_STEPS {
            let l1 = match self.get(leaf)? {
                PropertyIndexNodeRecord::Leaf { header, .. } => header.next_leaf,
                PropertyIndexNodeRecord::Internal { .. } => return None,
            };
            if l1.is_null() {
                leaf = match self.get(leaf)? {
                    PropertyIndexNodeRecord::Leaf { header, .. } => header.prev_leaf,
                    PropertyIndexNodeRecord::Internal { .. } => return None,
                };
                if leaf.is_null() {
                    return None;
                }
                continue;
            }
            let l2 = match self.get(l1)? {
                PropertyIndexNodeRecord::Leaf { header, .. } => header.next_leaf,
                PropertyIndexNodeRecord::Internal { .. } => return None,
            };
            if l2.is_null() {
                leaf = match self.get(leaf)? {
                    PropertyIndexNodeRecord::Leaf { header, .. } => header.prev_leaf,
                    PropertyIndexNodeRecord::Internal { .. } => return None,
                };
                if leaf.is_null() {
                    return None;
                }
                continue;
            }
            return Some(leaf);
        }
        None
    }

    fn load_ordered_three_leaf_window(
        &self,
        l0: PropertyIndexNodeId,
    ) -> Option<OrderedThreeLeafWindow> {
        let (e0, prev0, l1) = match self.get(l0)? {
            PropertyIndexNodeRecord::Leaf { header, entries } => {
                (entries.clone(), header.prev_leaf, header.next_leaf)
            }
            PropertyIndexNodeRecord::Internal { .. } => return None,
        };
        if l1.is_null() {
            return None;
        }
        let (e1, prev_l1, l2) = match self.get(l1)? {
            PropertyIndexNodeRecord::Leaf { header, entries } => {
                (entries.clone(), header.prev_leaf, header.next_leaf)
            }
            PropertyIndexNodeRecord::Internal { .. } => return None,
        };
        if l2.is_null() || prev_l1 != l0 {
            return None;
        }
        let (e2, prev_l2, next2) = match self.get(l2)? {
            PropertyIndexNodeRecord::Leaf { header, entries } => {
                (entries.clone(), header.prev_leaf, header.next_leaf)
            }
            PropertyIndexNodeRecord::Internal { .. } => return None,
        };
        if prev_l2 != l1 {
            return None;
        }
        Some(OrderedThreeLeafWindow {
            l0,
            l1,
            l2,
            prev0,
            next2,
            e0,
            e1,
            e2,
        })
    }

    /// Adjacent two-leaf redistribution failed: merge this leaf and its next two siblings,
    /// apply the insert, then repack with the same page-aware chunking as `from_index`.
    ///
    /// `leaf_in_window` is any leaf in the three-leaf span (left/middle/right); the merge always
    /// uses the leftmost leaf of that span as `l0`.
    ///
    /// Handles repartitions into one through many single-page leaves (same chunking as
    /// `from_index`). One chunk collapses three leaves into `l0`; two-leaf and three-leaf
    /// results adjust links and may drop a trailing sibling with internal repair; four or more
    /// extend the chain (extra allocates for five+) and rebuild internal levels over the leaf id
    /// list.
    fn try_upsert_three_leaf_redistribute(
        &mut self,
        leaf_in_window: PropertyIndexNodeId,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> bool {
        let Some(l0) = self.three_leaf_forward_window_start(leaf_in_window) else {
            return false;
        };
        let Some(win) = self.load_ordered_three_leaf_window(l0) else {
            return false;
        };
        let old_firsts = win.old_firsts();
        let mut merged = win.merged_entries();
        match merged.binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(i) => merged[i] = (key, entry),
            Err(i) => merged.insert(i, (key, entry)),
        }
        self.repartition_three_leaf_window_from_merged_entries(ThreeLeafRepartitionInput {
            l0: win.l0,
            l1: win.l1,
            l2: win.l2,
            prev0: win.prev0,
            next2: win.next2,
            old_firsts,
            merged,
        })
    }

    /// Pairwise borrow redistribution failed for an underfull leaf: merge three consecutive
    /// leaves, drop the key, then repack with the same page-aware chunking as insert-side
    /// three-leaf redistribution.
    fn try_remove_three_leaf_redistribute(&mut self, key: &PropertyIndexKey) -> bool {
        let Some((_, leaf_id)) = self.find_path_to_leaf_for_key(key) else {
            return false;
        };
        self.try_remove_three_leaf_redistribute_forward_window(leaf_id, key)
    }

    /// Same three-leaf anchoring as [`Self::try_upsert_three_leaf_redistribute`]: `leaf_in_window`
    /// may be the left, middle, or right leaf of the span; [`Self::three_leaf_forward_window_start`]
    /// finds `l0`, then entries are merged and the key removed before repartitioning.
    fn try_remove_three_leaf_redistribute_forward_window(
        &mut self,
        leaf_in_window: PropertyIndexNodeId,
        key: &PropertyIndexKey,
    ) -> bool {
        let Some(l0) = self.three_leaf_forward_window_start(leaf_in_window) else {
            return false;
        };
        let Some(win) = self.load_ordered_three_leaf_window(l0) else {
            return false;
        };
        let old_firsts = win.old_firsts();
        let mut merged = win.merged_entries();
        let Ok(index) = merged.binary_search_by(|(k, _)| k.cmp(key)) else {
            return true;
        };
        merged.remove(index);
        if merged.is_empty() {
            return false;
        }
        self.repartition_three_leaf_window_from_merged_entries(ThreeLeafRepartitionInput {
            l0: win.l0,
            l1: win.l1,
            l2: win.l2,
            prev0: win.prev0,
            next2: win.next2,
            old_firsts,
            merged,
        })
    }

    fn try_redistribute_insert_between_leaves(
        &mut self,
        left_leaf: PropertyIndexNodeId,
        right_leaf: PropertyIndexNodeId,
        left_prev_leaf: PropertyIndexNodeId,
        left_path: &[(PropertyIndexNodeId, usize)],
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> bool {
        let (left_entries, right_entries, right_next_leaf) =
            match (self.get(left_leaf), self.get(right_leaf)) {
                (
                    Some(PropertyIndexNodeRecord::Leaf {
                        entries: left_entries,
                        ..
                    }),
                    Some(PropertyIndexNodeRecord::Leaf {
                        header,
                        entries: right_entries,
                    }),
                ) => (
                    left_entries.clone(),
                    right_entries.clone(),
                    header.next_leaf,
                ),
                _ => return false,
            };

        let right_old_first = right_entries.first().map(|(first, _)| first.clone());
        let right_path = right_old_first
            .as_ref()
            .and_then(|first_key| self.find_path_to_leaf_for_key(first_key))
            .map(|(path, _)| path);

        let mut merged_entries = left_entries;
        merged_entries.extend(right_entries);
        match merged_entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
            Ok(index) => merged_entries[index] = (key, entry),
            Err(index) => merged_entries.insert(index, (key, entry)),
        }

        let split_at = self.find_leaf_redistribution_split(
            &merged_entries,
            left_prev_leaf,
            left_leaf,
            right_next_leaf,
        );
        let Some(split_at) = split_at else {
            return false;
        };

        let right_chunk = merged_entries.split_off(split_at);
        let left_chunk = merged_entries;
        let left_new_first = left_chunk.first().map(|(first, _)| first.clone());
        let right_new_first = right_chunk.first().map(|(first, _)| first.clone());

        self.nodes.insert(
            left_leaf,
            PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(left_chunk.len()).unwrap_or(u16::MAX),
                    left_prev_leaf,
                    right_leaf,
                ),
                entries: left_chunk,
            },
        );
        self.nodes.insert(
            right_leaf,
            PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(right_chunk.len()).unwrap_or(u16::MAX),
                    left_leaf,
                    right_next_leaf,
                ),
                entries: right_chunk,
            },
        );
        if !right_next_leaf.is_null() {
            let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(right_next_leaf)
            else {
                return false;
            };
            header.prev_leaf = right_leaf;
        }

        if let Some(new_first) = left_new_first {
            self.propagate_first_key_change(left_path, new_first);
        }
        if let (Some(old_first), Some(new_first), Some(right_path)) =
            (right_old_first, right_new_first, right_path)
            && old_first != new_first
        {
            self.propagate_first_key_change(&right_path, new_first);
        }

        true
    }

    /// Finds a bipartition of `merged_entries` into two **single-page** leaves (same constraint as
    /// [`Self::encode_node_page`] / [`Self::partition_entries_into_leaf_chunks`]) so pairwise
    /// insert redistribution can fall through to three-leaf repacking when no safe split exists.
    pub(crate) fn find_leaf_redistribution_split(
        &self,
        merged_entries: &[(PropertyIndexKey, PropertyIndexEntry)],
        left_prev_leaf: PropertyIndexNodeId,
        left_leaf: PropertyIndexNodeId,
        right_next_leaf: PropertyIndexNodeId,
    ) -> Option<usize> {
        if merged_entries.len() < 2 {
            return None;
        }
        let mid = merged_entries.len() / 2;
        let mut candidate_order = Vec::new();
        candidate_order.push(mid);
        for offset in 1..merged_entries.len() {
            if mid >= offset {
                candidate_order.push(mid - offset);
            }
            if mid + offset < merged_entries.len() {
                candidate_order.push(mid + offset);
            }
        }
        candidate_order
            .into_iter()
            .filter(|split_at| *split_at > 0 && *split_at < merged_entries.len())
            .find(|split_at| {
                let left_record = PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(*split_at).unwrap_or(u16::MAX),
                        left_prev_leaf,
                        left_leaf,
                    ),
                    entries: merged_entries[..*split_at].to_vec(),
                };
                let right_len = merged_entries.len() - *split_at;
                let right_record = PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(right_len).unwrap_or(u16::MAX),
                        left_leaf,
                        right_next_leaf,
                    ),
                    entries: merged_entries[*split_at..].to_vec(),
                };
                self.encode_node_page(&left_record).is_ok()
                    && self.encode_node_page(&right_record).is_ok()
            })
    }

    /// Removes one entry in-place when the node store is still in the single-leaf phase.
    ///
    /// Returns `true` when the persisted node store was updated incrementally.
    /// Returns `false` when the caller should fall back to rebuilding from the
    /// logical index.
    pub fn remove_single_leaf_entry(&mut self, key: &PropertyIndexKey) -> bool {
        let Some(leaf_id) = self.single_leaf_id() else {
            return self.nodes.is_empty();
        };
        let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.get_mut(leaf_id) else {
            return false;
        };
        let Ok(index) = entries.binary_search_by(|(existing, _)| existing.cmp(key)) else {
            return true;
        };
        entries.remove(index);
        if entries.is_empty() {
            let _ = self.nodes.remove(&leaf_id);
            self.free_node_ids.retain(|free_id| *free_id != leaf_id);
            self.allocator.next_node_id = 1;
            self.allocator.free_list_head = PropertyIndexNodeId::NULL;
            return true;
        }
        header.entry_count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
        header.prev_leaf = PropertyIndexNodeId::NULL;
        header.next_leaf = PropertyIndexNodeId::NULL;
        true
    }

    /// Removes one entry in-place when the node store is a leaf chain without internal nodes.
    ///
    /// Returns `true` when the persisted node store was updated incrementally.
    /// Returns `false` when the caller should fall back to rebuilding from the
    /// logical index.
    pub fn remove_leaf_chain_entry(&mut self, key: &PropertyIndexKey) -> bool {
        self.remove_leaf_chain_entry_with_kind(key).is_some()
    }

    /// Removes one entry and reports the incremental node-store path used.
    pub fn remove_leaf_chain_entry_with_kind(
        &mut self,
        key: &PropertyIndexKey,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        let out = self.remove_leaf_chain_entry_with_kind_inner(key);
        if out.is_some() {
            self.pidx_side_must_flush = true;
        }
        out
    }

    fn remove_leaf_chain_entry_with_kind_inner(
        &mut self,
        key: &PropertyIndexKey,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        if self.single_leaf_id().is_some() {
            let was_singleton = matches!(
                self.single_leaf_id().and_then(|leaf_id| self.get(leaf_id)),
                Some(PropertyIndexNodeRecord::Leaf { entries, .. }) if entries.len() == 1
            );
            return self
                .remove_single_leaf_entry(key)
                .then_some(if was_singleton {
                    PropertyIndexNodeStoreMutationKind::Collapse
                } else {
                    PropertyIndexNodeStoreMutationKind::LocalUpdate
                });
        }
        if self.try_remove_entry_with_empty_leaf_collapse(key) {
            return Some(PropertyIndexNodeStoreMutationKind::Collapse);
        }
        if let Some(kind) = self.try_remove_entry_with_leaf_redistribution(key) {
            return Some(kind);
        }
        if self.try_remove_entry_with_leaf_merge(key) {
            return Some(PropertyIndexNodeStoreMutationKind::Merge);
        }
        if self.try_remove_entry_locally(key) {
            return Some(PropertyIndexNodeStoreMutationKind::LocalUpdate);
        }
        let (leaf_ids, internal_ids, fanout) = self.incremental_leaf_chain_shape()?;
        let target_leaf_len = self.max_leaf_entry_count(&leaf_ids).max(1);
        let mut entries = self.collect_leaf_chain_entries(&leaf_ids);
        let Ok(index) = entries.binary_search_by(|(existing, _)| existing.cmp(key)) else {
            return Some(PropertyIndexNodeStoreMutationKind::LocalUpdate);
        };
        entries.remove(index);
        self.rewrite_leaf_chain_entries(leaf_ids, internal_ids, fanout, entries, target_leaf_len)
            .then_some(PropertyIndexNodeStoreMutationKind::Rebuild)
    }

    fn try_remove_entry_with_leaf_redistribution(
        &mut self,
        key: &PropertyIndexKey,
    ) -> Option<PropertyIndexNodeStoreMutationKind> {
        let (path, leaf_id) = self.find_path_to_leaf_for_key(key)?;
        let (leaf_ids, _, _) = self.incremental_leaf_chain_shape()?;
        let leaf_target_len = self.max_leaf_entry_count(&leaf_ids).max(1);
        let min_leaf_entries = leaf_target_len.div_ceil(2).max(1);

        let (entries_after_remove, prev_leaf, next_leaf) = match self.get(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                let mut cloned = entries.clone();
                let Ok(index) = cloned.binary_search_by(|(existing, _)| existing.cmp(key)) else {
                    return Some(PropertyIndexNodeStoreMutationKind::Redistribute);
                };
                cloned.remove(index);
                (cloned, header.prev_leaf, header.next_leaf)
            }
            Some(PropertyIndexNodeRecord::Internal { .. }) | None => return None,
        };

        if entries_after_remove.is_empty() || entries_after_remove.len() >= min_leaf_entries {
            return None;
        }

        if !next_leaf.is_null() {
            let next_old_first = self.first_key_for_subtree(next_leaf);
            let next_path = next_old_first
                .as_ref()
                .and_then(|first_key| self.find_path_to_leaf_for_key(first_key))
                .map(|(path, _)| path);
            if let (Some(next_old_first), Some(next_path)) = (next_old_first, next_path) {
                let next_state = match self.get(next_leaf) {
                    Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                        (header.next_leaf, entries.clone())
                    }
                    Some(PropertyIndexNodeRecord::Internal { .. }) | None => return None,
                };
                let (next_next_leaf, mut next_entries) = next_state;
                if next_entries.len() > min_leaf_entries {
                    let borrowed = next_entries.remove(0);
                    let mut current_entries = entries_after_remove.clone();
                    current_entries.push(borrowed);
                    let left_record = PropertyIndexNodeRecord::Leaf {
                        header: PropertyIndexNodeHeader::leaf(
                            u16::try_from(current_entries.len()).unwrap_or(u16::MAX),
                            prev_leaf,
                            next_leaf,
                        ),
                        entries: current_entries,
                    };
                    let right_record = PropertyIndexNodeRecord::Leaf {
                        header: PropertyIndexNodeHeader::leaf(
                            u16::try_from(next_entries.len()).unwrap_or(u16::MAX),
                            leaf_id,
                            next_next_leaf,
                        ),
                        entries: next_entries,
                    };
                    if self.encode_node_page(&left_record).is_ok()
                        && self.encode_node_page(&right_record).is_ok()
                    {
                        self.nodes.insert(leaf_id, left_record);
                        self.nodes.insert(next_leaf, right_record);
                        if let Some(new_first) = self.first_key_for_subtree(next_leaf)
                            && new_first != next_old_first
                        {
                            self.propagate_first_key_change(&next_path, new_first);
                        }
                        return Some(PropertyIndexNodeStoreMutationKind::Redistribute);
                    }
                }
            }
        }

        // Mirror insert-side `try_upsert_entry_with_leaf_redistribution`: try the forward (right)
        // sibling before the backward (left) one. Symmetric pairwise repair can still fail for both
        // (e.g. page-size encoding); the shared three-leaf path then anchors with
        // `three_leaf_forward_window_start` so middle/right targets use the same `l0 → l1 → l2` span.
        if !prev_leaf.is_null() {
            let prev_old_first = self.first_key_for_subtree(prev_leaf);
            let prev_path = prev_old_first
                .as_ref()
                .and_then(|first_key| self.find_path_to_leaf_for_key(first_key))
                .map(|(path, _)| path);
            if let Some(prev_path) = prev_path {
                let prev_state = match self.get(prev_leaf) {
                    Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                        (header.prev_leaf, entries.clone())
                    }
                    Some(PropertyIndexNodeRecord::Internal { .. }) | None => return None,
                };
                let (prev_prev_leaf, mut prev_entries) = prev_state;
                if prev_entries.len() > min_leaf_entries {
                    let borrowed = prev_entries.pop().expect("left sibling has spare entry");
                    let mut current_entries = entries_after_remove;
                    current_entries.insert(0, borrowed);
                    let left_record = PropertyIndexNodeRecord::Leaf {
                        header: PropertyIndexNodeHeader::leaf(
                            u16::try_from(prev_entries.len()).unwrap_or(u16::MAX),
                            prev_prev_leaf,
                            leaf_id,
                        ),
                        entries: prev_entries,
                    };
                    let right_record = PropertyIndexNodeRecord::Leaf {
                        header: PropertyIndexNodeHeader::leaf(
                            u16::try_from(current_entries.len()).unwrap_or(u16::MAX),
                            prev_leaf,
                            next_leaf,
                        ),
                        entries: current_entries,
                    };
                    if self.encode_node_page(&left_record).is_ok()
                        && self.encode_node_page(&right_record).is_ok()
                    {
                        self.nodes.insert(prev_leaf, left_record);
                        self.nodes.insert(leaf_id, right_record);
                        if let Some(new_first) = self.first_key_for_subtree(leaf_id) {
                            self.propagate_first_key_change(&path, new_first);
                        }
                        if let (Some(prev_old_first), Some(new_prev_first)) =
                            (prev_old_first, self.first_key_for_subtree(prev_leaf))
                            && prev_old_first != new_prev_first
                        {
                            self.propagate_first_key_change(&prev_path, new_prev_first);
                        }
                        return Some(PropertyIndexNodeStoreMutationKind::Redistribute);
                    }
                }
            }
        }

        self.try_remove_three_leaf_redistribute(key)
            .then_some(PropertyIndexNodeStoreMutationKind::ThreeLeafRepack)
    }

    /// Reconstructs one logical index from persisted leaf records.
    ///
    /// The current phase prefers the persisted leaf chain when it is available.
    /// If no usable leaf chain can be found, it falls back to rebuilding from
    /// all leaf payloads in node-id order.
    pub fn to_index(&self, branching_factor: u16) -> PropertyIndex {
        let mut index = PropertyIndex::new(branching_factor);
        let Some(first_leaf) = self.infer_first_leaf_id() else {
            return index;
        };

        let mut last_leaf = first_leaf;
        let mut visited = BTreeSet::new();
        let mut current = Some(first_leaf);
        while let Some(node_id) = current {
            if !visited.insert(node_id) {
                break;
            }
            let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.nodes.get(&node_id)
            else {
                break;
            };
            last_leaf = node_id;
            for (key, entry) in entries {
                index.insert(key.clone(), entry.clone());
            }
            current = (!header.next_leaf.is_null()).then_some(header.next_leaf);
        }

        if index.entries.is_empty() {
            for node_id in self.leaf_node_ids() {
                if let Some(PropertyIndexNodeRecord::Leaf { entries, .. }) =
                    self.nodes.get(&node_id)
                {
                    for (key, entry) in entries {
                        index.insert(key.clone(), entry.clone());
                    }
                }
            }
            if let Some(fallback_first) = self.leaf_node_ids().into_iter().next() {
                index.header.first_leaf = fallback_first;
                index.header.last_leaf = self
                    .leaf_node_ids()
                    .into_iter()
                    .last()
                    .unwrap_or(fallback_first);
            }
        } else {
            index.header.first_leaf = first_leaf;
            index.header.last_leaf = last_leaf;
        }
        index.header.root = self.infer_root_id(index.header.first_leaf);
        index
    }

    /// Returns entries matching one exact equality prefix by traversing the persisted tree shape.
    pub fn scan_value_prefix_direct(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
        encoded_value: &[u8],
    ) -> Vec<(PropertyIndexKey, PropertyIndexEntry)> {
        let target =
            PropertyIndexKey::lower_bound(entity_kind, property_name, encoded_value.to_vec());
        let Some(mut leaf_id) = self.find_leaf_for_key(&target) else {
            return Vec::new();
        };

        let mut visited = BTreeSet::new();
        let mut out = Vec::new();
        loop {
            if !visited.insert(leaf_id) {
                break;
            }
            let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.nodes.get(&leaf_id)
            else {
                break;
            };

            let mut saw_matching_prefix = false;
            let mut should_stop = false;
            for (key, entry) in entries {
                if key.matches_value_prefix(entity_kind, property_name, encoded_value) {
                    saw_matching_prefix = true;
                    out.push((key.clone(), entry.clone()));
                } else if saw_matching_prefix || key > &target {
                    should_stop = true;
                    if saw_matching_prefix {
                        break;
                    }
                }
            }

            if should_stop || header.next_leaf.is_null() {
                break;
            }
            leaf_id = header.next_leaf;
        }
        out
    }

    /// Returns entries matching one `(entity_kind, property_name)` prefix by traversing the persisted tree shape.
    pub fn scan_property_prefix_direct(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
    ) -> Vec<(PropertyIndexKey, PropertyIndexEntry)> {
        let target = PropertyIndexKey::property_lower_bound(entity_kind, property_name);
        let Some(mut leaf_id) = self.find_leaf_for_key(&target) else {
            return Vec::new();
        };

        let mut visited = BTreeSet::new();
        let mut out = Vec::new();
        loop {
            if !visited.insert(leaf_id) {
                break;
            }
            let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.nodes.get(&leaf_id)
            else {
                break;
            };

            let mut saw_matching_prefix = false;
            let mut should_stop = false;
            for (key, entry) in entries {
                if key.matches_property_prefix(entity_kind, property_name) {
                    saw_matching_prefix = true;
                    out.push((key.clone(), entry.clone()));
                } else if saw_matching_prefix || key > &target {
                    should_stop = true;
                    if saw_matching_prefix {
                        break;
                    }
                }
            }

            if should_stop || header.next_leaf.is_null() {
                break;
            }
            leaf_id = header.next_leaf;
        }
        out
    }

    pub(crate) fn leaf_node_ids(&self) -> Vec<PropertyIndexNodeId> {
        self.nodes
            .iter()
            .filter_map(|(node_id, record)| {
                matches!(record, PropertyIndexNodeRecord::Leaf { .. }).then_some(*node_id)
            })
            .collect()
    }

    fn single_leaf_id(&self) -> Option<PropertyIndexNodeId> {
        if self.nodes.is_empty() {
            return None;
        }
        if self.nodes.len() != 1 {
            return None;
        }
        self.nodes.iter().next().and_then(|(node_id, record)| {
            matches!(record, PropertyIndexNodeRecord::Leaf { .. }).then_some(*node_id)
        })
    }

    fn walk_ordered_leaf_chain_via_next_leaf(
        &self,
        first_leaf: PropertyIndexNodeId,
        expected_leaf_count: usize,
    ) -> Result<Vec<PropertyIndexNodeId>, PropertyIndexLeafChainShapeError> {
        let mut visited = BTreeSet::new();
        let mut out = Vec::with_capacity(expected_leaf_count);
        let mut current = Some(first_leaf);
        while let Some(node_id) = current {
            if !visited.insert(node_id) {
                return Err(PropertyIndexLeafChainShapeError::NextLeafCycle { at: node_id });
            }
            let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.nodes.get(&node_id)
            else {
                return Err(PropertyIndexLeafChainShapeError::NextLeafNotLeaf { at: node_id });
            };
            out.push(node_id);
            current = (!header.next_leaf.is_null()).then_some(header.next_leaf);
        }
        if out.len() != expected_leaf_count {
            return Err(PropertyIndexLeafChainShapeError::NextLeafChainLenMismatch {
                visited: out.len(),
                expected: expected_leaf_count,
            });
        }
        Ok(out)
    }

    fn ordered_leaf_chain_ids_without_internal_result(
        &self,
    ) -> Result<Vec<PropertyIndexNodeId>, PropertyIndexLeafChainShapeError> {
        if self
            .nodes
            .values()
            .any(|record| matches!(record, PropertyIndexNodeRecord::Internal { .. }))
        {
            return Err(PropertyIndexLeafChainShapeError::LeafOnlyStoreContainsInternalNode);
        }
        if self.nodes.is_empty() {
            return Ok(Vec::new());
        }
        let first = self
            .infer_first_leaf_id()
            .ok_or(PropertyIndexLeafChainShapeError::CannotInferFirstLeafInLeafOnlyStore)?;
        self.walk_ordered_leaf_chain_via_next_leaf(first, self.leaf_node_ids().len())
    }

    /// Returns the same data as [`Self::incremental_leaf_chain_shape`], or a structured error when
    /// the persisted shape is inconsistent (broken `next_leaf` chain, unreachable internal root, etc.).
    pub fn try_incremental_leaf_chain_shape(
        &self,
    ) -> Result<
        (Vec<PropertyIndexNodeId>, Vec<PropertyIndexNodeId>, usize),
        PropertyIndexLeafChainShapeError,
    > {
        let internal_ids: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(node_id, record)| {
                matches!(record, PropertyIndexNodeRecord::Internal { .. }).then_some(*node_id)
            })
            .collect();
        if internal_ids.is_empty() {
            let leaf_ids = self.ordered_leaf_chain_ids_without_internal_result()?;
            let fanout = leaf_ids.len().max(2);
            return Ok((leaf_ids, Vec::new(), fanout));
        }

        let leaf_ids = self.ordered_leaf_chain_ids_from_any_internal_root_result()?;
        let fanout = self
            .nodes
            .values()
            .filter_map(|record| match record {
                PropertyIndexNodeRecord::Internal { children, .. } => Some(children.len()),
                PropertyIndexNodeRecord::Leaf { .. } => None,
            })
            .max()
            .unwrap_or(2)
            .max(2);
        Ok((leaf_ids, internal_ids, fanout))
    }

    fn incremental_leaf_chain_shape(
        &self,
    ) -> Option<(Vec<PropertyIndexNodeId>, Vec<PropertyIndexNodeId>, usize)> {
        self.try_incremental_leaf_chain_shape().ok()
    }

    fn max_leaf_entry_count(&self, leaf_ids: &[PropertyIndexNodeId]) -> usize {
        leaf_ids
            .iter()
            .filter_map(|leaf_id| match self.nodes.get(leaf_id) {
                Some(PropertyIndexNodeRecord::Leaf { entries, .. }) => Some(entries.len()),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn partition_entries_into_leaf_chunks(
        &self,
        entries: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
    ) -> Result<Vec<Vec<(PropertyIndexKey, PropertyIndexEntry)>>, PropertyIndexError> {
        let mut chunks = Vec::new();
        let mut current = Vec::new();

        for entry in entries {
            current.push(entry);
            if current.len() == 1 {
                continue;
            }
            let tentative = PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(current.len()).unwrap_or(u16::MAX),
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: current.clone(),
            };
            if self.encode_node_page(&tentative).is_err() {
                let last = current.pop().expect("current leaf chunk is non-empty");
                chunks.push(current);
                current = vec![last];
            }
        }

        if !current.is_empty() {
            chunks.push(current);
        }

        if chunks.is_empty() {
            chunks.push(Vec::new());
        }

        for chunk in &chunks {
            if chunk.is_empty() {
                continue;
            }
            let tentative = PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(chunk.len()).unwrap_or(u16::MAX),
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: chunk.clone(),
            };
            if self.encode_node_page(&tentative).is_err() {
                if chunk.len() != 1 {
                    return Err(PropertyIndexError::LeafPartitionMultiEntryExceedsPrimaryPage);
                }
                if self.encode_node_pages(&tentative).is_err() {
                    return Err(PropertyIndexError::LeafPartitionSingletonNotEncodable);
                }
            }
        }

        Ok(chunks)
    }

    fn build_internal_levels_from_leaf_chain(
        &mut self,
        leaf_ids: &[PropertyIndexNodeId],
        fanout: usize,
    ) -> Option<PropertyIndexNodeId> {
        if leaf_ids.len() <= 1 {
            return leaf_ids.first().copied();
        }

        let fanout = fanout.max(2);
        let mut current_level = leaf_ids.to_vec();
        while current_level.len() > 1 {
            let mut next_level = Vec::new();
            for children in current_level.chunks(fanout) {
                if children.len() == 1 {
                    next_level.push(children[0]);
                    continue;
                }
                let keys: Vec<_> = children
                    .iter()
                    .skip(1)
                    .filter_map(|child_id| self.first_key_for_subtree(*child_id))
                    .collect();
                if keys.len() + 1 != children.len() {
                    return None;
                }
                let node_id = self.allocate(PropertyIndexNodeRecord::Internal {
                    header: PropertyIndexNodeHeader::internal_with_capacity(
                        u16::try_from(keys.len()).unwrap_or(u16::MAX),
                        u16::try_from(fanout).unwrap_or(u16::MAX),
                    ),
                    keys,
                    children: children.to_vec(),
                });
                next_level.push(node_id);
            }
            current_level = next_level;
        }
        current_level.first().copied()
    }

    fn try_upsert_entry_locally(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> bool {
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(&key) else {
            return false;
        };
        let leaf_capacity = self.max_leaf_entry_count(&self.leaf_node_ids()).max(1);
        let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.get(leaf_id) else {
            return false;
        };
        let mut updated_entries = entries.clone();
        let old_first = updated_entries.first().map(|(first, _)| first.clone());
        match updated_entries.binary_search_by(|(existing, _)| existing.cmp(&key)) {
            Ok(index) => updated_entries[index] = (key.clone(), entry),
            Err(index) => {
                if updated_entries.len() >= leaf_capacity {
                    return false;
                }
                updated_entries.insert(index, (key.clone(), entry));
            }
        }
        let tentative = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                u16::try_from(updated_entries.len()).unwrap_or(u16::MAX),
                header.prev_leaf,
                header.next_leaf,
            ),
            entries: updated_entries,
        };
        // Match `partition_entries_into_leaf_chunks` / `from_index`: a leaf in this shape
        // should fit one node page so rebuild and incremental paths stay aligned.
        if self.encode_node_page(&tentative).is_err() {
            return false;
        }
        let PropertyIndexNodeRecord::Leaf {
            header: new_header,
            entries: new_entries,
        } = tentative
        else {
            return false;
        };
        let Some(record) = self.get_mut(leaf_id) else {
            return false;
        };
        let PropertyIndexNodeRecord::Leaf {
            header,
            entries: dest_entries,
        } = record
        else {
            return false;
        };
        *header = new_header;
        *dest_entries = new_entries;
        let new_first = dest_entries.first().map(|(first, _)| first.clone());
        if old_first != new_first
            && let Some(new_first) = new_first
        {
            self.propagate_first_key_change(&path, new_first);
        }
        true
    }

    fn try_upsert_entry_with_leaf_split(
        &mut self,
        key: PropertyIndexKey,
        entry: PropertyIndexEntry,
    ) -> bool {
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(&key) else {
            return false;
        };
        let Some((mut leaf_ids, internal_ids, fanout)) = self.incremental_leaf_chain_shape() else {
            return false;
        };
        let leaf_index = match leaf_ids.iter().position(|existing| *existing == leaf_id) {
            Some(index) => index,
            None => return false,
        };

        let (merged_entries, prev_leaf, next_leaf) = match self.get(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                let mut merged = entries.clone();
                match merged.binary_search_by(|(existing, _)| existing.cmp(&key)) {
                    Ok(index) => merged[index] = (key, entry),
                    Err(index) => merged.insert(index, (key, entry)),
                }
                (merged, header.prev_leaf, header.next_leaf)
            }
            Some(PropertyIndexNodeRecord::Internal { .. }) | None => return false,
        };

        let merged_tentative = PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                u16::try_from(merged_entries.len()).unwrap_or(u16::MAX),
                prev_leaf,
                next_leaf,
            ),
            entries: merged_entries,
        };
        if self.encode_node_page(&merged_tentative).is_ok() {
            return false;
        }
        let PropertyIndexNodeRecord::Leaf {
            entries: merged_entries,
            ..
        } = merged_tentative
        else {
            unreachable!("merged_tentative is always a leaf record");
        };

        let Ok(chunks) = self.partition_entries_into_leaf_chunks(merged_entries) else {
            return false;
        };
        if chunks.len() != 2 {
            return false;
        }
        let mut chunk_iter = chunks.into_iter();
        let left_entries = chunk_iter.next().expect("chunks length checked");
        let right_entries = chunk_iter.next().expect("chunks length checked");

        for chunk in [&left_entries, &right_entries] {
            let t = PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(chunk.len()).unwrap_or(u16::MAX),
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: (*chunk).clone(),
            };
            if self.encode_node_page(&t).is_err() {
                return false;
            }
        }

        let right_leaf = self.allocate(PropertyIndexNodeRecord::Leaf {
            header: PropertyIndexNodeHeader::leaf(
                u16::try_from(right_entries.len()).unwrap_or(u16::MAX),
                leaf_id,
                next_leaf,
            ),
            entries: right_entries,
        });

        if let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.get_mut(leaf_id) {
            header.prev_leaf = prev_leaf;
            header.next_leaf = right_leaf;
            header.entry_count = u16::try_from(left_entries.len()).unwrap_or(u16::MAX);
            *entries = left_entries;
        } else {
            return false;
        }

        if !next_leaf.is_null() {
            if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next_leaf) {
                header.prev_leaf = right_leaf;
            } else {
                return false;
            }
        }

        leaf_ids.insert(leaf_index + 1, right_leaf);
        if !self.try_attach_split_leaf_to_parent(&path, right_leaf)
            && !self.try_attach_split_leaf_via_ancestor_splits(&path, right_leaf)
        {
            self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids, fanout);
        }

        if let Some(new_first) = self.first_key_for_subtree(leaf_id) {
            self.propagate_first_key_change(&path, new_first);
        }
        true
    }

    fn try_attach_split_leaf_to_parent(
        &mut self,
        path: &[(PropertyIndexNodeId, usize)],
        right_leaf: PropertyIndexNodeId,
    ) -> bool {
        let Some((parent_id, child_index)) = path.last().copied() else {
            return false;
        };
        let (capacity, mut children) = match self.get(parent_id) {
            Some(PropertyIndexNodeRecord::Internal {
                header, children, ..
            }) => (usize::from(header.capacity.max(2)), children.clone()),
            Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
        };
        if children.len() >= capacity {
            return false;
        }
        if child_index >= children.len() {
            return false;
        }

        children.insert(child_index + 1, right_leaf);
        let keys: Vec<_> = children
            .iter()
            .skip(1)
            .filter_map(|child_id| self.first_key_for_subtree(*child_id))
            .collect();
        if keys.len() + 1 != children.len() {
            return false;
        }

        self.nodes.insert(
            parent_id,
            PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(
                    u16::try_from(keys.len()).unwrap_or(u16::MAX),
                    u16::try_from(capacity).unwrap_or(u16::MAX),
                ),
                keys,
                children,
            },
        );
        true
    }

    fn try_attach_split_leaf_via_ancestor_splits(
        &mut self,
        path: &[(PropertyIndexNodeId, usize)],
        right_leaf: PropertyIndexNodeId,
    ) -> bool {
        if path.is_empty() {
            return false;
        }

        let mut pending_right = right_leaf;

        for (depth, (node_id, child_index)) in path.iter().copied().enumerate().rev() {
            let (capacity, mut children) = match self.get(node_id) {
                Some(PropertyIndexNodeRecord::Internal {
                    header, children, ..
                }) => (usize::from(header.capacity.max(2)), children.clone()),
                Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
            };
            if child_index >= children.len() {
                return false;
            }

            children.insert(child_index + 1, pending_right);
            if children.len() <= capacity {
                let Some((keys, children)) = self.build_internal_keys_and_children(children) else {
                    return false;
                };
                self.nodes.insert(
                    node_id,
                    PropertyIndexNodeRecord::Internal {
                        header: PropertyIndexNodeHeader::internal_with_capacity(
                            u16::try_from(keys.len()).unwrap_or(u16::MAX),
                            u16::try_from(capacity).unwrap_or(u16::MAX),
                        ),
                        keys,
                        children,
                    },
                );
                return true;
            }

            let split_at = children.len() / 2;
            let right_children = children.split_off(split_at);
            if children.len() < 2 || right_children.len() < 2 {
                return false;
            }

            let Some((left_keys, left_children)) = self.build_internal_keys_and_children(children)
            else {
                return false;
            };
            let Some((right_keys, right_children)) =
                self.build_internal_keys_and_children(right_children)
            else {
                return false;
            };

            let right_node_id = self.allocate(PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(
                    u16::try_from(right_keys.len()).unwrap_or(u16::MAX),
                    u16::try_from(capacity).unwrap_or(u16::MAX),
                ),
                keys: right_keys,
                children: right_children,
            });
            self.nodes.insert(
                node_id,
                PropertyIndexNodeRecord::Internal {
                    header: PropertyIndexNodeHeader::internal_with_capacity(
                        u16::try_from(left_keys.len()).unwrap_or(u16::MAX),
                        u16::try_from(capacity).unwrap_or(u16::MAX),
                    ),
                    keys: left_keys,
                    children: left_children,
                },
            );

            pending_right = right_node_id;
            if depth == 0 {
                let Some((root_keys, root_children)) =
                    self.build_internal_keys_and_children(vec![node_id, pending_right])
                else {
                    return false;
                };
                let root_capacity = path
                    .first()
                    .and_then(|(root_id, _)| match self.get(*root_id) {
                        Some(PropertyIndexNodeRecord::Internal { header, .. }) => {
                            Some(usize::from(header.capacity.max(2)))
                        }
                        Some(PropertyIndexNodeRecord::Leaf { .. }) | None => None,
                    })
                    .unwrap_or(2)
                    .max(2);
                let _ = self.allocate(PropertyIndexNodeRecord::Internal {
                    header: PropertyIndexNodeHeader::internal_with_capacity(
                        u16::try_from(root_keys.len()).unwrap_or(u16::MAX),
                        u16::try_from(root_capacity).unwrap_or(u16::MAX),
                    ),
                    keys: root_keys,
                    children: root_children,
                });
                return true;
            }
        }

        false
    }

    fn build_internal_keys_and_children(
        &self,
        children: Vec<PropertyIndexNodeId>,
    ) -> Option<(Vec<PropertyIndexKey>, Vec<PropertyIndexNodeId>)> {
        let keys: Vec<_> = children
            .iter()
            .skip(1)
            .filter_map(|child_id| self.first_key_for_subtree(*child_id))
            .collect();
        (keys.len() + 1 == children.len()).then_some((keys, children))
    }

    fn try_remove_entry_locally(&mut self, key: &PropertyIndexKey) -> bool {
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(key) else {
            return false;
        };
        let Some(PropertyIndexNodeRecord::Leaf { header, entries }) = self.get_mut(leaf_id) else {
            return false;
        };
        let Ok(index) = entries.binary_search_by(|(existing, _)| existing.cmp(key)) else {
            return true;
        };
        if entries.len() == 1 {
            return false;
        }
        let old_first = entries.first().map(|(first, _)| first.clone());
        entries.remove(index);
        header.entry_count = u16::try_from(entries.len()).unwrap_or(u16::MAX);
        let new_first = entries.first().map(|(first, _)| first.clone());
        if old_first != new_first
            && let Some(new_first) = new_first
        {
            self.propagate_first_key_change(&path, new_first);
        }
        true
    }

    fn try_remove_entry_with_empty_leaf_collapse(&mut self, key: &PropertyIndexKey) -> bool {
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(key) else {
            return false;
        };
        let Some((mut leaf_ids, internal_ids, fanout)) = self.incremental_leaf_chain_shape() else {
            return false;
        };
        let leaf_index = match leaf_ids.iter().position(|existing| *existing == leaf_id) {
            Some(index) => index,
            None => return false,
        };

        let (entries_after_remove, prev_leaf, next_leaf) = match self.get(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                let mut cloned = entries.clone();
                let Ok(index) = cloned.binary_search_by(|(existing, _)| existing.cmp(key)) else {
                    return true;
                };
                cloned.remove(index);
                (cloned, header.prev_leaf, header.next_leaf)
            }
            Some(PropertyIndexNodeRecord::Internal { .. }) | None => return false,
        };

        if !entries_after_remove.is_empty() {
            return false;
        }

        if !prev_leaf.is_null() {
            let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(prev_leaf) else {
                return false;
            };
            header.next_leaf = next_leaf;
        }
        if !next_leaf.is_null() {
            let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = self.get_mut(next_leaf) else {
                return false;
            };
            header.prev_leaf = prev_leaf;
        }

        leaf_ids.remove(leaf_index);
        let _ = self.free(leaf_id);
        if !self.try_remove_child_via_ancestor_compaction(&path, leaf_id) {
            self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids, fanout);
        }
        true
    }

    fn try_remove_entry_with_leaf_merge(&mut self, key: &PropertyIndexKey) -> bool {
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(key) else {
            return false;
        };
        let Some((mut leaf_ids, internal_ids, fanout)) = self.incremental_leaf_chain_shape() else {
            return false;
        };
        let leaf_index = match leaf_ids.iter().position(|existing| *existing == leaf_id) {
            Some(index) => index,
            None => return false,
        };

        let (entries_after_remove, prev_leaf, next_leaf) = match self.get(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                let mut cloned = entries.clone();
                let Ok(index) = cloned.binary_search_by(|(existing, _)| existing.cmp(key)) else {
                    return true;
                };
                cloned.remove(index);
                (cloned, header.prev_leaf, header.next_leaf)
            }
            Some(PropertyIndexNodeRecord::Internal { .. }) | None => return false,
        };

        if entries_after_remove.is_empty() {
            return false;
        }

        if !next_leaf.is_null() {
            let next_leaf_first_key = self.first_key_for_subtree(next_leaf);
            let (next_next_leaf, next_entries) = match self.get(next_leaf) {
                Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                    (header.next_leaf, entries.clone())
                }
                Some(PropertyIndexNodeRecord::Internal { .. }) | None => return false,
            };
            let mut merged_entries = entries_after_remove.clone();
            merged_entries.extend(next_entries);
            let merged_record = PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(merged_entries.len()).unwrap_or(u16::MAX),
                    prev_leaf,
                    next_next_leaf,
                ),
                entries: merged_entries,
            };
            if self.encode_node_page(&merged_record).is_ok() {
                self.nodes.insert(leaf_id, merged_record);
                if !next_next_leaf.is_null() {
                    let Some(PropertyIndexNodeRecord::Leaf { header, .. }) =
                        self.get_mut(next_next_leaf)
                    else {
                        return false;
                    };
                    header.prev_leaf = leaf_id;
                }
                leaf_ids.remove(leaf_index + 1);
                let _ = self.free(next_leaf);
                let updated = next_leaf_first_key
                    .and_then(|first_key| self.find_path_to_leaf_for_key(&first_key))
                    .map(|(next_path, _)| {
                        self.try_remove_child_via_ancestor_compaction(&next_path, next_leaf)
                    })
                    .unwrap_or(false);
                if !updated {
                    self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids, fanout);
                }
                return true;
            }
        }

        if !prev_leaf.is_null() {
            let (prev_prev_leaf, prev_entries) = match self.get(prev_leaf) {
                Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                    (header.prev_leaf, entries.clone())
                }
                Some(PropertyIndexNodeRecord::Internal { .. }) | None => return false,
            };
            let mut merged_entries = prev_entries;
            merged_entries.extend(entries_after_remove);
            let merged_record = PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    u16::try_from(merged_entries.len()).unwrap_or(u16::MAX),
                    prev_prev_leaf,
                    next_leaf,
                ),
                entries: merged_entries,
            };
            if self.encode_node_page(&merged_record).is_ok() {
                self.nodes.insert(prev_leaf, merged_record);
                if !next_leaf.is_null() {
                    let Some(PropertyIndexNodeRecord::Leaf { header, .. }) =
                        self.get_mut(next_leaf)
                    else {
                        return false;
                    };
                    header.prev_leaf = prev_leaf;
                }
                leaf_ids.remove(leaf_index);
                let _ = self.free(leaf_id);
                if !self.try_remove_child_via_ancestor_compaction(&path, leaf_id) {
                    self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids, fanout);
                }
                return true;
            }
        }

        false
    }

    fn propagate_first_key_change(
        &mut self,
        path: &[(PropertyIndexNodeId, usize)],
        new_first: PropertyIndexKey,
    ) {
        for (node_id, child_index) in path.iter().rev() {
            let Some(PropertyIndexNodeRecord::Internal { keys, .. }) = self.get_mut(*node_id)
            else {
                return;
            };
            if *child_index > 0 {
                let separator_index = child_index - 1;
                if let Some(separator) = keys.get_mut(separator_index) {
                    *separator = new_first;
                }
                return;
            }
        }
    }

    fn try_remove_child_via_ancestor_compaction(
        &mut self,
        path: &[(PropertyIndexNodeId, usize)],
        removed_child: PropertyIndexNodeId,
    ) -> bool {
        if path.is_empty() {
            return false;
        }

        let mut pending_old = removed_child;
        let mut pending_replacement = None;
        for depth in (0..path.len()).rev() {
            let node_id = path[depth].0;
            let Some(PropertyIndexNodeRecord::Internal {
                header, children, ..
            }) = self.get(node_id)
            else {
                return false;
            };
            let capacity = usize::from(header.capacity.max(2));
            let mut new_children = children.clone();
            let Some(update_index) = new_children.iter().position(|child| *child == pending_old)
            else {
                return false;
            };
            match pending_replacement {
                Some(replacement) => new_children[update_index] = replacement,
                None => {
                    new_children.remove(update_index);
                }
            }

            if new_children.is_empty() {
                return false;
            }

            let min_children = Self::min_internal_children(capacity);
            if depth > 0 && new_children.len() < min_children {
                if !self.rewrite_internal_node(node_id, new_children.clone(), capacity) {
                    return false;
                }
                if self.try_repair_underfull_internal_at_depth(path, depth) {
                    return true;
                }
                if new_children.len() == 1 {
                    let replacement = new_children[0];
                    let _ = self.free(node_id);
                    pending_old = node_id;
                    pending_replacement = Some(replacement);
                    continue;
                }
                return false;
            }

            if new_children.len() == 1 {
                let replacement = new_children[0];
                let _ = self.free(node_id);
                pending_old = node_id;
                pending_replacement = Some(replacement);
                continue;
            }

            let Some((keys, children)) = self.build_internal_keys_and_children(new_children) else {
                return false;
            };
            self.nodes.insert(
                node_id,
                PropertyIndexNodeRecord::Internal {
                    header: PropertyIndexNodeHeader::internal_with_capacity(
                        u16::try_from(keys.len()).unwrap_or(u16::MAX),
                        u16::try_from(capacity).unwrap_or(u16::MAX),
                    ),
                    keys,
                    children,
                },
            );

            if update_index == 0
                && let Some(new_first) = self.first_key_for_subtree(node_id)
            {
                self.propagate_first_key_change(&path[..depth], new_first);
            }
            return true;
        }

        true
    }

    fn min_internal_children(capacity: usize) -> usize {
        capacity.max(2).div_ceil(2).max(2)
    }

    fn rewrite_internal_node(
        &mut self,
        node_id: PropertyIndexNodeId,
        children: Vec<PropertyIndexNodeId>,
        capacity: usize,
    ) -> bool {
        let Some((keys, children)) = self.build_internal_keys_and_children(children) else {
            return false;
        };
        self.nodes.insert(
            node_id,
            PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(
                    u16::try_from(keys.len()).unwrap_or(u16::MAX),
                    u16::try_from(capacity).unwrap_or(u16::MAX),
                ),
                keys,
                children,
            },
        );
        true
    }

    fn try_repair_underfull_internal_at_depth(
        &mut self,
        path: &[(PropertyIndexNodeId, usize)],
        depth: usize,
    ) -> bool {
        if depth == 0 {
            return true;
        }

        let node_id = path[depth].0;
        let parent_id = path[depth - 1].0;
        let (node_capacity, mut node_children) = match self.get(node_id) {
            Some(PropertyIndexNodeRecord::Internal {
                header, children, ..
            }) => (usize::from(header.capacity.max(2)), children.clone()),
            Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
        };
        let min_children = Self::min_internal_children(node_capacity);
        if node_children.len() >= min_children {
            return true;
        }

        let parent_children = match self.get(parent_id) {
            Some(PropertyIndexNodeRecord::Internal { children, .. }) => children.clone(),
            Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
        };
        let Some(parent_pos) = parent_children.iter().position(|child| *child == node_id) else {
            return false;
        };

        if let Some(right_sibling_id) = parent_children.get(parent_pos + 1).copied() {
            let (right_capacity, mut right_children) = match self.get(right_sibling_id) {
                Some(PropertyIndexNodeRecord::Internal {
                    header, children, ..
                }) => (usize::from(header.capacity.max(2)), children.clone()),
                Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
            };
            if right_children.len() > Self::min_internal_children(right_capacity) {
                let borrowed = right_children.remove(0);
                node_children.push(borrowed);
                if !self.rewrite_internal_node(node_id, node_children, node_capacity)
                    || !self.rewrite_internal_node(right_sibling_id, right_children, right_capacity)
                {
                    return false;
                }
                return self.refresh_parent_after_internal_child_update(path, depth - 1);
            }
        }

        if parent_pos > 0 {
            let left_sibling_id = parent_children[parent_pos - 1];
            let (left_capacity, mut left_children) = match self.get(left_sibling_id) {
                Some(PropertyIndexNodeRecord::Internal {
                    header, children, ..
                }) => (usize::from(header.capacity.max(2)), children.clone()),
                Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
            };
            if left_children.len() > Self::min_internal_children(left_capacity) {
                let borrowed = left_children
                    .pop()
                    .expect("left sibling has one spare child");
                node_children.insert(0, borrowed);
                if !self.rewrite_internal_node(left_sibling_id, left_children, left_capacity)
                    || !self.rewrite_internal_node(node_id, node_children, node_capacity)
                {
                    return false;
                }
                return self.refresh_parent_after_internal_child_update(path, depth - 1);
            }
        }

        if let Some(right_sibling_id) = parent_children.get(parent_pos + 1).copied() {
            let (right_capacity, right_children) = match self.get(right_sibling_id) {
                Some(PropertyIndexNodeRecord::Internal {
                    header, children, ..
                }) => (usize::from(header.capacity.max(2)), children.clone()),
                Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
            };
            let mut merged_children = node_children.clone();
            merged_children.extend(right_children);
            if merged_children.len() <= node_capacity.max(right_capacity) {
                let target_capacity = node_capacity.max(right_capacity);
                if !self.rewrite_internal_node(node_id, merged_children, target_capacity) {
                    return false;
                }
                let _ = self.free(right_sibling_id);
                return self
                    .try_remove_child_via_ancestor_compaction(&path[..depth], right_sibling_id);
            }
        }

        if parent_pos > 0 {
            let left_sibling_id = parent_children[parent_pos - 1];
            let (left_capacity, left_children) = match self.get(left_sibling_id) {
                Some(PropertyIndexNodeRecord::Internal {
                    header, children, ..
                }) => (usize::from(header.capacity.max(2)), children.clone()),
                Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
            };
            let mut merged_children = left_children;
            merged_children.extend(node_children);
            if merged_children.len() <= left_capacity.max(node_capacity) {
                let target_capacity = left_capacity.max(node_capacity);
                if !self.rewrite_internal_node(left_sibling_id, merged_children, target_capacity) {
                    return false;
                }
                let _ = self.free(node_id);
                return self.try_remove_child_via_ancestor_compaction(&path[..depth], node_id);
            }
        }

        false
    }

    fn refresh_parent_after_internal_child_update(
        &mut self,
        path: &[(PropertyIndexNodeId, usize)],
        parent_depth: usize,
    ) -> bool {
        let parent_id = path[parent_depth].0;
        let (capacity, children) = match self.get(parent_id) {
            Some(PropertyIndexNodeRecord::Internal {
                header, children, ..
            }) => (usize::from(header.capacity.max(2)), children.clone()),
            Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
        };
        let child_count = children.len();
        if !self.rewrite_internal_node(parent_id, children, capacity) {
            return false;
        }
        if parent_depth > 0
            && child_count < Self::min_internal_children(capacity)
            && !self.try_repair_underfull_internal_at_depth(path, parent_depth)
        {
            return false;
        }
        if let Some(new_first) = self.first_key_for_subtree(parent_id) {
            self.propagate_first_key_change(&path[..parent_depth], new_first);
        }
        true
    }

    pub(crate) fn first_key_for_subtree(
        &self,
        node_id: PropertyIndexNodeId,
    ) -> Option<PropertyIndexKey> {
        let leaf_id = self.leftmost_leaf_from_node(node_id)?;
        match self.nodes.get(&leaf_id)? {
            PropertyIndexNodeRecord::Leaf { entries, .. } => {
                entries.first().map(|(key, _)| key.clone())
            }
            PropertyIndexNodeRecord::Internal { .. } => None,
        }
    }

    fn collect_leaf_chain_entries(
        &self,
        leaf_ids: &[PropertyIndexNodeId],
    ) -> Vec<(PropertyIndexKey, PropertyIndexEntry)> {
        let mut out = Vec::new();
        for leaf_id in leaf_ids {
            if let Some(PropertyIndexNodeRecord::Leaf { entries, .. }) = self.nodes.get(leaf_id) {
                out.extend(entries.iter().cloned());
            }
        }
        out
    }

    fn rewrite_leaf_chain_entries(
        &mut self,
        mut leaf_ids: Vec<PropertyIndexNodeId>,
        internal_ids: Vec<PropertyIndexNodeId>,
        fanout: usize,
        entries: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
        _target_leaf_len: usize,
    ) -> bool {
        if entries.is_empty() {
            for leaf_id in leaf_ids {
                let _ = self.free(leaf_id);
            }
            for internal_id in internal_ids {
                let _ = self.free(internal_id);
            }
            if self.nodes.is_empty() {
                self.free_node_ids.clear();
                self.allocator.next_node_id = 1;
                self.allocator.free_list_head = PropertyIndexNodeId::NULL;
            }
            return true;
        }

        let Ok(chunks) = self.partition_entries_into_leaf_chunks(entries) else {
            return false;
        };

        while leaf_ids.len() < chunks.len() {
            let new_leaf = self.allocate(PropertyIndexNodeRecord::Leaf {
                header: PropertyIndexNodeHeader::leaf(
                    0,
                    PropertyIndexNodeId::NULL,
                    PropertyIndexNodeId::NULL,
                ),
                entries: Vec::new(),
            });
            leaf_ids.push(new_leaf);
        }
        while leaf_ids.len() > chunks.len() {
            let Some(extra_leaf) = leaf_ids.pop() else {
                break;
            };
            let _ = self.free(extra_leaf);
        }

        for (index, chunk) in chunks.into_iter().enumerate() {
            let leaf_id = leaf_ids[index];
            let prev_leaf = if index == 0 {
                PropertyIndexNodeId::NULL
            } else {
                leaf_ids[index - 1]
            };
            let next_leaf = leaf_ids
                .get(index + 1)
                .copied()
                .unwrap_or(PropertyIndexNodeId::NULL);
            self.nodes.insert(
                leaf_id,
                PropertyIndexNodeRecord::Leaf {
                    header: PropertyIndexNodeHeader::leaf(
                        u16::try_from(chunk.len()).unwrap_or(u16::MAX),
                        prev_leaf,
                        next_leaf,
                    ),
                    entries: chunk,
                },
            );
        }
        self.rebuild_internal_levels_over_leaf_chain(internal_ids, &leaf_ids, fanout);
        true
    }

    fn rebuild_internal_levels_over_leaf_chain(
        &mut self,
        internal_ids: Vec<PropertyIndexNodeId>,
        leaf_ids: &[PropertyIndexNodeId],
        fanout: usize,
    ) {
        if self.try_rebuild_single_internal_root_over_leaf_chain(&internal_ids, leaf_ids, fanout) {
            return;
        }
        for internal_id in internal_ids {
            let _ = self.free(internal_id);
        }
        let _ = self.build_internal_levels_from_leaf_chain(leaf_ids, fanout);
    }

    fn try_rebuild_single_internal_root_over_leaf_chain(
        &mut self,
        internal_ids: &[PropertyIndexNodeId],
        leaf_ids: &[PropertyIndexNodeId],
        _fanout: usize,
    ) -> bool {
        if internal_ids.len() != 1 {
            return false;
        }
        let root_id = internal_ids[0];
        let capacity = match self.get(root_id) {
            Some(PropertyIndexNodeRecord::Internal { header, .. }) => {
                usize::from(header.capacity.max(2))
            }
            Some(PropertyIndexNodeRecord::Leaf { .. }) | None => return false,
        };

        if leaf_ids.len() <= 1 {
            let _ = self.free(root_id);
            return true;
        }
        if leaf_ids.len() > capacity {
            return false;
        }

        let keys: Vec<_> = leaf_ids
            .iter()
            .skip(1)
            .filter_map(|child_id| self.first_key_for_subtree(*child_id))
            .collect();
        if keys.len() + 1 != leaf_ids.len() {
            return false;
        }

        self.nodes.insert(
            root_id,
            PropertyIndexNodeRecord::Internal {
                header: PropertyIndexNodeHeader::internal_with_capacity(
                    u16::try_from(keys.len()).unwrap_or(u16::MAX),
                    u16::try_from(capacity).unwrap_or(u16::MAX),
                ),
                keys,
                children: leaf_ids.to_vec(),
            },
        );
        true
    }

    fn infer_root_node_id(&self) -> Option<PropertyIndexNodeId> {
        let internal_ids: BTreeSet<_> = self
            .nodes
            .iter()
            .filter_map(|(node_id, record)| {
                matches!(record, PropertyIndexNodeRecord::Internal { .. }).then_some(*node_id)
            })
            .collect();
        if internal_ids.is_empty() {
            return None;
        }

        let referenced_internal_ids: BTreeSet<_> = self
            .nodes
            .values()
            .filter_map(|record| match record {
                PropertyIndexNodeRecord::Internal { children, .. } => Some(children),
                PropertyIndexNodeRecord::Leaf { .. } => None,
            })
            .flat_map(|children| children.iter().copied())
            .filter(|child_id| internal_ids.contains(child_id))
            .collect();

        internal_ids
            .iter()
            .find(|node_id| !referenced_internal_ids.contains(node_id))
            .copied()
            .or_else(|| internal_ids.iter().next().copied())
    }

    fn ordered_leaf_chain_ids_from_any_internal_root_result(
        &self,
    ) -> Result<Vec<PropertyIndexNodeId>, PropertyIndexLeafChainShapeError> {
        let root_id = self
            .infer_root_node_id()
            .ok_or(PropertyIndexLeafChainShapeError::InternalRootMissing)?;
        let first_leaf = self.leftmost_leaf_from_root(root_id).ok_or(
            PropertyIndexLeafChainShapeError::InternalLeftmostLeafUnreachable { root: root_id },
        )?;
        self.walk_ordered_leaf_chain_via_next_leaf(first_leaf, self.leaf_node_ids().len())
    }

    fn find_leaf_for_key(&self, target: &PropertyIndexKey) -> Option<PropertyIndexNodeId> {
        let mut current = self
            .infer_root_node_id()
            .or_else(|| self.infer_first_leaf_id())?;
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(current) {
                return None;
            }
            match self.nodes.get(&current)? {
                PropertyIndexNodeRecord::Leaf { .. } => return Some(current),
                PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                    let child_index = Self::select_child_for_key(keys, children.len(), target);
                    current = *children.get(child_index)?;
                }
            }
        }
    }

    pub(crate) fn select_child_for_key(
        keys: &[PropertyIndexKey],
        child_count: usize,
        target: &PropertyIndexKey,
    ) -> usize {
        let idx = keys.partition_point(|key| key <= target);
        idx.min(child_count.saturating_sub(1))
    }

    fn leftmost_leaf_from_node(&self, root_id: PropertyIndexNodeId) -> Option<PropertyIndexNodeId> {
        let mut visited = BTreeSet::new();
        let mut current = root_id;
        loop {
            if !visited.insert(current) {
                return None;
            }
            match self.nodes.get(&current)? {
                PropertyIndexNodeRecord::Leaf { .. } => return Some(current),
                PropertyIndexNodeRecord::Internal { children, .. } => {
                    current = *children.first()?;
                }
            }
        }
    }

    fn leftmost_leaf_from_root(&self, root_id: PropertyIndexNodeId) -> Option<PropertyIndexNodeId> {
        self.leftmost_leaf_from_node(root_id)
    }

    fn infer_first_leaf_id(&self) -> Option<PropertyIndexNodeId> {
        if let Some(root_id) = self.infer_root_node_id()
            && let Some(leaf_id) = self.leftmost_leaf_from_root(root_id)
        {
            return Some(leaf_id);
        }
        self.nodes
            .iter()
            .find_map(|(node_id, record)| match record {
                PropertyIndexNodeRecord::Leaf { header, .. } if header.prev_leaf.is_null() => {
                    Some(*node_id)
                }
                _ => None,
            })
            .or_else(|| self.leaf_node_ids().into_iter().next())
    }

    fn infer_root_id(&self, first_leaf: PropertyIndexNodeId) -> PropertyIndexNodeId {
        self.infer_root_node_id().unwrap_or(first_leaf)
    }

    fn find_path_to_leaf_for_key(
        &self,
        target: &PropertyIndexKey,
    ) -> Option<(Vec<(PropertyIndexNodeId, usize)>, PropertyIndexNodeId)> {
        let mut current = self
            .infer_root_node_id()
            .or_else(|| self.infer_first_leaf_id())?;
        let mut visited = BTreeSet::new();
        let mut path = Vec::new();
        loop {
            if !visited.insert(current) {
                return None;
            }
            match self.nodes.get(&current)? {
                PropertyIndexNodeRecord::Leaf { .. } => return Some((path, current)),
                PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                    let child_index = Self::select_child_for_key(keys, children.len(), target);
                    path.push((current, child_index));
                    current = *children.get(child_index)?;
                }
            }
        }
    }

    /// Returns the fixed byte offset of one node page inside a paged node area.
    ///
    /// Node ids are interpreted as stable page slots. `NULL` is not a valid page.
    pub fn node_page_offset(
        &self,
        node_id: PropertyIndexNodeId,
    ) -> Result<u64, PropertyIndexError> {
        if node_id.is_null() {
            return Err(PropertyIndexError::NullNodeId);
        }
        let page_size = u64::from(self.allocator.page_size_bytes);
        node_id
            .0
            .checked_sub(1)
            .and_then(|index| index.checked_mul(page_size))
            .ok_or(PropertyIndexError::LengthOverflow)
    }

    /// Encodes one node record as a fixed-size page.
    ///
    /// The current phase requires each node record to fit in a single node page.
    /// Multi-page overflow is a later step.
    pub fn encode_node_page(
        &self,
        node: &PropertyIndexNodeRecord,
    ) -> Result<Vec<u8>, PropertyIndexError> {
        let payload = node.encode()?;
        let page_size = usize::try_from(self.allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let pages = self.encode_node_pages(node)?;
        if pages.len() != 1 {
            return Err(PropertyIndexError::NodeTooLarge {
                encoded_len: Self::NODE_PAGE_HEADER_LEN
                    .checked_add(payload.len())
                    .ok_or(PropertyIndexError::LengthOverflow)?,
                page_size,
            });
        }
        Ok(pages.into_iter().next().expect("single page"))
    }

    /// Decodes one node record from a fixed-size page.
    pub fn decode_node_page(
        &self,
        page: &[u8],
    ) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
        self.decode_node_pages(&[page.to_vec()])
    }

    /// Encodes one node record to an initial page plus zero or more overflow pages.
    pub fn encode_node_pages(
        &self,
        node: &PropertyIndexNodeRecord,
    ) -> Result<Vec<Vec<u8>>, PropertyIndexError> {
        let payload = node.encode()?;
        let page_size = usize::try_from(self.allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        if page_size <= Self::NODE_PAGE_HEADER_LEN
            || page_size <= Self::NODE_OVERFLOW_PAGE_HEADER_LEN
        {
            return Err(PropertyIndexError::NodePageTooSmall(page_size));
        }

        let first_capacity = page_size - Self::NODE_PAGE_HEADER_LEN;
        let overflow_capacity = page_size - Self::NODE_OVERFLOW_PAGE_HEADER_LEN;
        let overflow_count = if payload.len() <= first_capacity {
            0usize
        } else {
            (payload.len() - first_capacity).div_ceil(overflow_capacity)
        };
        let total_pages = 1 + overflow_count;
        let mut pages = vec![vec![0u8; page_size]; total_pages];

        let first_len = first_capacity.min(payload.len());
        pages[0][0..4].copy_from_slice(&Self::NODE_PAGE_MAGIC);
        pages[0][4] = Self::NODE_PAGE_VERSION;
        pages[0][5..9].copy_from_slice(
            &u32::try_from(payload.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        let first_next = if overflow_count == 0 { 0u64 } else { 1u64 };
        pages[0][9..17].copy_from_slice(&first_next.to_le_bytes());
        pages[0][Self::NODE_PAGE_HEADER_LEN..Self::NODE_PAGE_HEADER_LEN + first_len]
            .copy_from_slice(&payload[..first_len]);

        let mut offset = first_len;
        for (page_index, page) in pages.iter_mut().enumerate().take(total_pages).skip(1) {
            let remaining = payload.len() - offset;
            let len = overflow_capacity.min(remaining);
            page[0..4].copy_from_slice(&Self::NODE_OVERFLOW_PAGE_MAGIC);
            page[4] = Self::NODE_OVERFLOW_PAGE_VERSION;
            let next = if page_index + 1 < total_pages {
                (page_index + 1) as u64
            } else {
                0
            };
            page[5..13].copy_from_slice(&next.to_le_bytes());
            page[Self::NODE_OVERFLOW_PAGE_HEADER_LEN..Self::NODE_OVERFLOW_PAGE_HEADER_LEN + len]
                .copy_from_slice(&payload[offset..offset + len]);
            offset += len;
        }

        Ok(pages)
    }

    /// Decodes one node record from an initial page plus zero or more overflow pages.
    pub fn decode_node_pages(
        &self,
        pages: &[Vec<u8>],
    ) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
        let page_size = usize::try_from(self.allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        if pages.is_empty() {
            return Err(PropertyIndexError::RecordTooShort(0));
        }
        for page in pages {
            if page.len() != page_size {
                return Err(PropertyIndexError::InvalidNodePageLength(page.len()));
            }
        }
        let first = &pages[0];
        if first[..4] != Self::NODE_PAGE_MAGIC {
            return Err(PropertyIndexError::InvalidNodePageMagic(
                first[..4].to_vec(),
            ));
        }
        if first[4] != Self::NODE_PAGE_VERSION {
            return Err(PropertyIndexError::UnsupportedNodePageVersion(first[4]));
        }
        let mut payload_len = [0u8; 4];
        payload_len.copy_from_slice(&first[5..9]);
        let payload_len = u32::from_le_bytes(payload_len) as usize;
        let mut next = [0u8; 8];
        next.copy_from_slice(&first[9..17]);
        let mut next_index = u64::from_le_bytes(next);
        let mut payload = Vec::with_capacity(payload_len);
        let first_available = page_size - Self::NODE_PAGE_HEADER_LEN;
        let first_len = first_available.min(payload_len);
        payload.extend_from_slice(
            &first[Self::NODE_PAGE_HEADER_LEN..Self::NODE_PAGE_HEADER_LEN + first_len],
        );

        while payload.len() < payload_len {
            if next_index == 0 {
                return Err(PropertyIndexError::TruncatedNodeOverflowChain {
                    expected_payload_len: payload_len,
                    decoded_payload_len: payload.len(),
                });
            }
            let page_index =
                usize::try_from(next_index).map_err(|_| PropertyIndexError::LengthOverflow)?;
            let page = pages
                .get(page_index)
                .ok_or(PropertyIndexError::MissingOverflowPage(page_index))?;
            if page[..4] != Self::NODE_OVERFLOW_PAGE_MAGIC {
                return Err(PropertyIndexError::InvalidOverflowPageMagic(
                    page[..4].to_vec(),
                ));
            }
            if page[4] != Self::NODE_OVERFLOW_PAGE_VERSION {
                return Err(PropertyIndexError::UnsupportedOverflowPageVersion(page[4]));
            }
            let mut overflow_next = [0u8; 8];
            overflow_next.copy_from_slice(&page[5..13]);
            next_index = u64::from_le_bytes(overflow_next);
            let remaining = payload_len - payload.len();
            let len = (page_size - Self::NODE_OVERFLOW_PAGE_HEADER_LEN).min(remaining);
            payload.extend_from_slice(
                &page[Self::NODE_OVERFLOW_PAGE_HEADER_LEN
                    ..Self::NODE_OVERFLOW_PAGE_HEADER_LEN + len],
            );
        }

        PropertyIndexNodeRecord::decode(&payload)
    }

    fn parse_paged_area_layout_prefix(bytes: &[u8]) -> Result<PagedAreaParsedPrefix, PropertyIndexError> {
        let min_len = 4 + 1 + PropertyIndexAllocatorHeader::ENCODED_LEN + 4 + 8;
        if bytes.len() < min_len {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        if bytes[..4] != Self::PAGED_AREA_MAGIC {
            return Err(PropertyIndexError::InvalidPagedAreaMagic(
                bytes[..4].to_vec(),
            ));
        }
        let version = bytes[4];
        let allocator_start = 5;
        let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
        let allocator =
            PropertyIndexAllocatorHeader::decode(&bytes[allocator_start..allocator_end])?;
        let mut free_count = [0u8; 4];
        free_count.copy_from_slice(&bytes[allocator_end..allocator_end + 4]);
        let free_count = u32::from_le_bytes(free_count) as usize;
        let mut page_count = [0u8; 8];
        page_count.copy_from_slice(&bytes[allocator_end + 4..allocator_end + 12]);
        let page_count = u64::from_le_bytes(page_count) as usize;
        let (overflow_page_count, mut offset) = match version {
            1 => (0usize, allocator_end + 12),
            2 => {
                let mut overflow_page_count = [0u8; 8];
                overflow_page_count.copy_from_slice(&bytes[allocator_end + 12..allocator_end + 20]);
                (
                    u64::from_le_bytes(overflow_page_count) as usize,
                    allocator_end + 20,
                )
            }
            other => return Err(PropertyIndexError::UnsupportedPagedAreaVersion(other)),
        };

        let mut free_node_ids = Vec::with_capacity(free_count);
        for _ in 0..free_count {
            if bytes.len().saturating_sub(offset) < 8 {
                return Err(PropertyIndexError::RecordTooShort(
                    bytes.len().saturating_sub(offset),
                ));
            }
            let mut free_id = [0u8; 8];
            free_id.copy_from_slice(&bytes[offset..offset + 8]);
            free_node_ids.push(PropertyIndexNodeId(u64::from_le_bytes(free_id)));
            offset += 8;
        }

        let page_size = usize::try_from(allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let pages_start = offset;
        let expected = pages_start
            .checked_add(
                page_count
                    .checked_add(overflow_page_count)
                    .ok_or(PropertyIndexError::LengthOverflow)?
                    .checked_mul(page_size)
                    .ok_or(PropertyIndexError::LengthOverflow)?,
            )
            .ok_or(PropertyIndexError::LengthOverflow)?;
        if expected != bytes.len() {
            return Err(PropertyIndexError::RecordLengthMismatch {
                expected,
                actual: bytes.len(),
            });
        }

        Ok(PagedAreaParsedPrefix {
            version,
            allocator,
            free_node_ids,
            page_count,
            overflow_page_count,
            pages_start,
            page_size,
        })
    }

    fn read_paged_slot_slice<'a>(
        bytes: &'a [u8],
        pages_start: usize,
        page_size: usize,
        total_slots: usize,
        slot_index: usize,
    ) -> Result<&'a [u8], PropertyIndexError> {
        if slot_index >= total_slots {
            return Err(PropertyIndexError::MissingOverflowPage(slot_index));
        }
        let off = pages_start
            .checked_add(
                slot_index
                    .checked_mul(page_size)
                    .ok_or(PropertyIndexError::LengthOverflow)?,
            )
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let end = off
            .checked_add(page_size)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        bytes
            .get(off..end)
            .ok_or(PropertyIndexError::RecordTooShort(bytes.len()))
    }

    /// Per initial slot (node id `index + 1`), total page count including overflow chain.
    fn page_counts_per_initial_slot_from_bytes(
        bytes: &[u8],
        layout: &PagedAreaParsedPrefix,
    ) -> Result<Vec<usize>, PropertyIndexError> {
        let page_count = layout.page_count;
        let ps = layout.page_size;
        let base = layout.pages_start;
        let total_slots = page_count
            .checked_add(layout.overflow_page_count)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let mut out = Vec::with_capacity(page_count);
        for initial_index in 0..page_count {
            let page =
                Self::read_paged_slot_slice(bytes, base, ps, total_slots, initial_index)?;
            if page.iter().all(|b| *b == 0) {
                out.push(1);
                continue;
            }
            if layout.version < 2 {
                out.push(1);
                continue;
            }
            let mut count = 1usize;
            let mut next = u64::from_le_bytes(
                page[9..17]
                    .try_into()
                    .map_err(|_| PropertyIndexError::RecordTooShort(page.len()))?,
            );
            while next != 0 {
                let gi = usize::try_from(next).map_err(|_| PropertyIndexError::LengthOverflow)?;
                let op = Self::read_paged_slot_slice(bytes, base, ps, total_slots, gi)?;
                count += 1;
                next = u64::from_le_bytes(
                    op[5..13]
                        .try_into()
                        .map_err(|_| PropertyIndexError::RecordTooShort(op.len()))?,
                );
            }
            out.push(count);
        }
        Ok(out)
    }

    fn global_slot_indices_for_page_counts(counts: &[usize]) -> Vec<Vec<usize>> {
        let page_count = counts.len();
        let mut ov = page_count;
        let mut out = Vec::with_capacity(page_count);
        for raw in 0..page_count {
            let c = counts[raw];
            let mut g = Vec::with_capacity(c);
            g.push(raw);
            for _ in 1..c {
                g.push(ov);
                ov += 1;
            }
            out.push(g);
        }
        out
    }

    fn link_paged_pages_to_global_indices(
        mut pages: Vec<Vec<u8>>,
        global_slots: &[usize],
    ) -> Result<Vec<Vec<u8>>, PropertyIndexError> {
        if pages.len() != global_slots.len() {
            return Err(PropertyIndexError::RecordLengthMismatch {
                expected: pages.len(),
                actual: global_slots.len(),
            });
        }
        if pages.len() == 1 {
            pages[0][9..17].copy_from_slice(&0u64.to_le_bytes());
            return Ok(pages);
        }
        pages[0][9..17].copy_from_slice(&(global_slots[1] as u64).to_le_bytes());
        for i in 1..pages.len() {
            let next_g = if i + 1 < pages.len() {
                global_slots[i + 1] as u64
            } else {
                0
            };
            pages[i][5..13].copy_from_slice(&next_g.to_le_bytes());
        }
        Ok(pages)
    }

    /// Attempts to build the next paged area by re-reading the previous on-disk bytes and patching
    /// only changed slots. Falls back to [`Self::encode_paged_area`] when layout or version differs.
    ///
    /// Compared to [`Self::encode_paged_area`], this avoids allocating one cleared `page_count`-long
    /// initial slot table when most slots are empty (sparse property index under a large
    /// `next_node_id`).
    pub(crate) fn try_encode_paged_area_incremental(
        &self,
        old_bytes: &[u8],
    ) -> Result<Option<Vec<u8>>, PropertyIndexError> {
        let layout = match Self::parse_paged_area_layout_prefix(old_bytes) {
            Ok(l) => l,
            Err(_) => return Ok(None),
        };
        if layout.version != Self::PAGED_AREA_VERSION {
            return Ok(None);
        }
        if self.allocator != layout.allocator || self.free_node_ids != layout.free_node_ids {
            return Ok(None);
        }
        let counts_disk = match Self::page_counts_per_initial_slot_from_bytes(old_bytes, &layout) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        if counts_disk.len() != layout.page_count {
            return Ok(None);
        }
        let overflow_from_counts: usize = counts_disk.iter().map(|c| c.saturating_sub(1)).sum();
        if overflow_from_counts != layout.overflow_page_count {
            return Ok(None);
        }

        let ps = layout.page_size;
        let base = layout.pages_start;
        let page_count = layout.page_count;
        let global_indices = Self::global_slot_indices_for_page_counts(&counts_disk);

        let mut out: Option<Vec<u8>> = None;

        for raw in 1..=page_count {
            let idx = raw - 1;
            let node_id = PropertyIndexNodeId(raw as u64);
            let gslots = &global_indices[idx];
            let expected_pages = counts_disk[idx];

            if let Some(node) = self.nodes.get(&node_id) {
                let raw_pages = self.encode_node_pages(node)?;
                if raw_pages.len() != expected_pages {
                    return Ok(None);
                }
                let finalized = Self::link_paged_pages_to_global_indices(raw_pages, gslots)?;
                let mut node_mismatch = false;
                for (page, &gidx) in finalized.iter().zip(gslots.iter()) {
                    let off = base
                        .checked_add(
                            gidx
                                .checked_mul(ps)
                                .ok_or(PropertyIndexError::LengthOverflow)?,
                        )
                        .ok_or(PropertyIndexError::LengthOverflow)?;
                    let disk = &old_bytes[off..off + ps];
                    if disk != page.as_slice() {
                        node_mismatch = true;
                        break;
                    }
                }
                if node_mismatch {
                    if out.is_none() {
                        out = Some(old_bytes.to_vec());
                    }
                    let buf = out.as_mut().expect("just set");
                    for (page, &gidx) in finalized.iter().zip(gslots.iter()) {
                        let off = base
                            .checked_add(
                                gidx
                                    .checked_mul(ps)
                                    .ok_or(PropertyIndexError::LengthOverflow)?,
                            )
                            .ok_or(PropertyIndexError::LengthOverflow)?;
                        buf[off..off + ps].copy_from_slice(page.as_slice());
                    }
                }
            } else {
                if expected_pages != 1 {
                    return Ok(None);
                }
                let off = base
                    .checked_add(idx.checked_mul(ps).ok_or(PropertyIndexError::LengthOverflow)?)
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                let disk = &old_bytes[off..off + ps];
                if !disk.iter().all(|b| *b == 0) {
                    if out.is_none() {
                        out = Some(old_bytes.to_vec());
                    }
                    let buf = out.as_mut().expect("just set");
                    buf[off..off + ps].fill(0);
                }
            }
        }

        Ok(Some(out.unwrap_or_else(|| old_bytes.to_vec())))
    }

    /// When the on-disk area has **no overflow pages**, both free lists are empty, and this store
    /// only **grows** [`PropertyIndexAllocatorHeader::next_node_id`] (fresh tail allocations),
    /// append new initial slots by copying the old prefix and encoding the tail — avoiding a full
    /// [`Self::encode_paged_area`] walk over every historical slot.
    ///
    /// Returns [`None`] when existing initial pages changed (splits), overflow is present, or the
    /// free list is in use.
    pub(crate) fn try_encode_paged_area_zero_overflow_tail_extend(
        &self,
        old_bytes: &[u8],
    ) -> Result<Option<Vec<u8>>, PropertyIndexError> {
        let layout = match Self::parse_paged_area_layout_prefix(old_bytes) {
            Ok(l) => l,
            Err(_) => return Ok(None),
        };
        if layout.version != Self::PAGED_AREA_VERSION || layout.overflow_page_count != 0 {
            return Ok(None);
        }
        if !self.free_node_ids.is_empty() || !layout.free_node_ids.is_empty() {
            return Ok(None);
        }
        let old_alloc = layout.allocator;
        if self.allocator.page_size_bytes != old_alloc.page_size_bytes
            || self.allocator.reserved != old_alloc.reserved
            || self.allocator.free_list_head != old_alloc.free_list_head
            || self.allocator.next_node_id <= old_alloc.next_node_id
        {
            return Ok(None);
        }
        let ps = layout.page_size;
        let base = layout.pages_start;
        let old_pc = layout.page_count;
        let page_count_from_alloc = usize::try_from(old_alloc.next_node_id.saturating_sub(1))
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        if page_count_from_alloc != old_pc {
            return Ok(None);
        }
        let new_pc_u64 = self.allocator.next_node_id.saturating_sub(1);
        let new_pc = usize::try_from(new_pc_u64).map_err(|_| PropertyIndexError::LengthOverflow)?;
        if new_pc <= old_pc {
            return Ok(None);
        }

        for raw in 1..=old_pc {
            let idx = raw - 1;
            let off = base
                .checked_add(idx.checked_mul(ps).ok_or(PropertyIndexError::LengthOverflow)?)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let disk = &old_bytes[off..off + ps];
            let encoded = if let Some(node) = self.nodes.get(&PropertyIndexNodeId(raw as u64)) {
                let pages = self.encode_node_pages(node)?;
                if pages.len() != 1 {
                    return Ok(None);
                }
                pages.into_iter().next().expect("one page")
            } else {
                vec![0u8; ps]
            };
            if disk != encoded.as_slice() {
                return Ok(None);
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(&Self::PAGED_AREA_MAGIC);
        out.push(Self::PAGED_AREA_VERSION);
        out.extend_from_slice(&self.allocator.encode());
        out.extend_from_slice(
            &u32::try_from(self.free_node_ids.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        out.extend_from_slice(&new_pc_u64.to_le_bytes());
        out.extend_from_slice(&0u64.to_le_bytes());
        for free_id in &self.free_node_ids {
            out.extend_from_slice(&free_id.0.to_le_bytes());
        }
        let pages_start = out.len();
        let body_len = new_pc
            .checked_mul(ps)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        out.resize(
            pages_start
                .checked_add(body_len)
                .ok_or(PropertyIndexError::LengthOverflow)?,
            0u8,
        );
        let old_init_len = old_pc
            .checked_mul(ps)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        out[pages_start..pages_start + old_init_len]
            .copy_from_slice(&old_bytes[base..base + old_init_len]);

        for raw in (old_pc + 1)..=new_pc {
            let idx = raw - 1;
            let dst_start = pages_start
                .checked_add(idx.checked_mul(ps).ok_or(PropertyIndexError::LengthOverflow)?)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let dst = &mut out[dst_start..dst_start + ps];
            if let Some(node) = self.nodes.get(&PropertyIndexNodeId(raw as u64)) {
                let pages = self.encode_node_pages(node)?;
                if pages.len() != 1 {
                    return Ok(None);
                }
                dst.copy_from_slice(&pages[0]);
            } else {
                dst.fill(0);
            }
        }

        Ok(Some(out))
    }

    /// Encodes this node store as a fixed-slot paged area.
    ///
    /// Each non-null node id owns exactly one initial page slot in the area.
    /// Additional overflow pages remain embedded in the slot payload for now.
    pub fn encode_paged_area(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let page_size = usize::try_from(self.allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let page_count = self.allocator.next_node_id.saturating_sub(1);
        let page_count_usize =
            usize::try_from(page_count).map_err(|_| PropertyIndexError::LengthOverflow)?;
        let initial_byte_len = page_count_usize
            .checked_mul(page_size)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        // One contiguous zeroed slab for all initial slots (avoids `page_count` small Vec
        // allocations when the property index is sparse under a large `next_node_id`).
        let mut initial_flat = vec![0u8; initial_byte_len];
        let mut overflow_slots: Vec<Vec<u8>> = Vec::new();

        for raw_node_id in 1..=page_count {
            let node_id = PropertyIndexNodeId(raw_node_id);
            let Some(node) = self.nodes.get(&node_id) else {
                continue;
            };
            let mut pages = self.encode_node_pages(node)?;
            let initial_index =
                usize::try_from(raw_node_id - 1).map_err(|_| PropertyIndexError::LengthOverflow)?;
            let total_node_pages = pages.len();
            let first_page = if total_node_pages == 1 {
                pages.pop().expect("one page")
            } else {
                let mut first = std::mem::replace(&mut pages[0], Vec::new());
                let first_overflow_slot = page_count
                    .checked_add(
                        u64::try_from(overflow_slots.len())
                            .map_err(|_| PropertyIndexError::LengthOverflow)?,
                    )
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                first[9..17].copy_from_slice(&first_overflow_slot.to_le_bytes());
                for (overflow_idx, mut overflow_page) in pages.into_iter().enumerate().skip(1) {
                    let next_global = if overflow_idx + 1 < total_node_pages {
                        page_count
                            .checked_add(
                                u64::try_from(overflow_slots.len() + 1)
                                    .map_err(|_| PropertyIndexError::LengthOverflow)?,
                            )
                            .ok_or(PropertyIndexError::LengthOverflow)?
                    } else {
                        0
                    };
                    overflow_page[5..13].copy_from_slice(&next_global.to_le_bytes());
                    overflow_slots.push(overflow_page);
                }
                first
            };
            let dst_start = initial_index
                .checked_mul(page_size)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let dst_end = dst_start
                .checked_add(page_size)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            initial_flat[dst_start..dst_end].copy_from_slice(&first_page);
        }

        let mut out = Vec::new();
        out.extend_from_slice(&Self::PAGED_AREA_MAGIC);
        out.push(Self::PAGED_AREA_VERSION);
        out.extend_from_slice(&self.allocator.encode());
        out.extend_from_slice(
            &u32::try_from(self.free_node_ids.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        out.extend_from_slice(&page_count.to_le_bytes());
        out.extend_from_slice(
            &u64::try_from(overflow_slots.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        for free_id in &self.free_node_ids {
            out.extend_from_slice(&free_id.0.to_le_bytes());
        }
        out.extend_from_slice(&initial_flat);
        for slot in overflow_slots {
            out.extend_from_slice(&slot);
        }
        Ok(out)
    }

    /// Decodes one fixed-slot paged node-store area.
    pub fn decode_paged_area(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        let min_len_v1 = 4 + 1 + PropertyIndexAllocatorHeader::ENCODED_LEN + 4 + 8;
        let min_len = min_len_v1;
        if bytes.len() < min_len {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        if bytes[..4] != Self::PAGED_AREA_MAGIC {
            return Err(PropertyIndexError::InvalidPagedAreaMagic(
                bytes[..4].to_vec(),
            ));
        }
        let version = bytes[4];

        let allocator_start = 5;
        let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
        let allocator =
            PropertyIndexAllocatorHeader::decode(&bytes[allocator_start..allocator_end])?;
        let mut free_count = [0u8; 4];
        free_count.copy_from_slice(&bytes[allocator_end..allocator_end + 4]);
        let free_count = u32::from_le_bytes(free_count) as usize;
        let mut page_count = [0u8; 8];
        page_count.copy_from_slice(&bytes[allocator_end + 4..allocator_end + 12]);
        let page_count = u64::from_le_bytes(page_count) as usize;
        let (overflow_page_count, mut offset) = match version {
            1 => (0usize, allocator_end + 12),
            2 => {
                let mut overflow_page_count = [0u8; 8];
                overflow_page_count.copy_from_slice(&bytes[allocator_end + 12..allocator_end + 20]);
                (
                    u64::from_le_bytes(overflow_page_count) as usize,
                    allocator_end + 20,
                )
            }
            other => return Err(PropertyIndexError::UnsupportedPagedAreaVersion(other)),
        };

        let mut free_node_ids = Vec::with_capacity(free_count);
        for _ in 0..free_count {
            if bytes.len().saturating_sub(offset) < 8 {
                return Err(PropertyIndexError::RecordTooShort(
                    bytes.len().saturating_sub(offset),
                ));
            }
            let mut free_id = [0u8; 8];
            free_id.copy_from_slice(&bytes[offset..offset + 8]);
            free_node_ids.push(PropertyIndexNodeId(u64::from_le_bytes(free_id)));
            offset += 8;
        }

        let page_size = usize::try_from(allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let expected = offset
            .checked_add(
                page_count
                    .checked_add(overflow_page_count)
                    .ok_or(PropertyIndexError::LengthOverflow)?
                    .checked_mul(page_size)
                    .ok_or(PropertyIndexError::LengthOverflow)?,
            )
            .ok_or(PropertyIndexError::LengthOverflow)?;
        if expected != bytes.len() {
            return Err(PropertyIndexError::RecordLengthMismatch {
                expected,
                actual: bytes.len(),
            });
        }

        let mut nodes = BTreeMap::new();
        let helper = Self {
            allocator,
            free_node_ids: free_node_ids.clone(),
            nodes: BTreeMap::new(),
            pidx_side_must_flush: false,
        };
        let total_slots = page_count
            .checked_add(overflow_page_count)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let pages_start = offset;
        let read_slot = |slot_index: usize| -> Result<Vec<u8>, PropertyIndexError> {
            if slot_index >= total_slots {
                return Err(PropertyIndexError::MissingOverflowPage(slot_index));
            }
            let page_start = pages_start
                .checked_add(
                    slot_index
                        .checked_mul(page_size)
                        .ok_or(PropertyIndexError::LengthOverflow)?,
                )
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let page_end = page_start + page_size;
            Ok(bytes[page_start..page_end].to_vec())
        };
        for index in 0..page_count {
            let page = read_slot(index)?;
            if page.iter().all(|byte| *byte == 0) {
                continue;
            }
            let mut pages = vec![page];
            if version >= 2 {
                let mut next = [0u8; 8];
                next.copy_from_slice(&pages[0][9..17]);
                let mut next_index = u64::from_le_bytes(next);
                while next_index != 0 {
                    let global_index = usize::try_from(next_index)
                        .map_err(|_| PropertyIndexError::LengthOverflow)?;
                    pages.push(read_slot(global_index)?);
                    let last = pages.last().expect("overflow page");
                    let mut overflow_next = [0u8; 8];
                    overflow_next.copy_from_slice(&last[5..13]);
                    next_index = u64::from_le_bytes(overflow_next);
                }
            }
            let record = helper.decode_node_pages(&pages)?;
            nodes.insert(PropertyIndexNodeId((index + 1) as u64), record);
        }

        Ok(Self {
            allocator,
            free_node_ids,
            nodes,
            pidx_side_must_flush: false,
        })
    }

    pub(crate) fn paged_area_pages_offset(
        version: u8,
        free_count: usize,
    ) -> Result<usize, PropertyIndexError> {
        let fixed_len = match version {
            1 => 4 + 1 + PropertyIndexAllocatorHeader::ENCODED_LEN + 4 + 8,
            2 => Self::PAGED_AREA_FIXED_HEADER_LEN,
            other => return Err(PropertyIndexError::UnsupportedPagedAreaVersion(other)),
        };
        fixed_len
            .checked_add(
                free_count
                    .checked_mul(8)
                    .ok_or(PropertyIndexError::LengthOverflow)?,
            )
            .ok_or(PropertyIndexError::LengthOverflow)
    }
}
