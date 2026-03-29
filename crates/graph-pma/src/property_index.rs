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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use gleaph_graph_kernel::{EdgeId, NodeId};

use crate::low_level::{RegionKind, RegionManager, RegionStorageKind, WASM_PAGE_SIZE};
use crate::stable::Memory;
use crate::stable::{Bound, Storable};

/// Stable discriminant for node or edge property-index entries.
///
/// Invariant:
/// - node and edge index entries must never share the same stable prefix
/// - the discriminant is always the first byte of an encoded key
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyIndexEntityKind {
    /// Equality index for node properties.
    VertexNode = b'N',
    /// Equality index for edge properties.
    VertexEdge = b'E',
}

impl PropertyIndexEntityKind {
    /// Returns the one-byte stable encoding tag.
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// Decodes one entity kind from a stable one-byte tag.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            b'N' => Some(Self::VertexNode),
            b'E' => Some(Self::VertexEdge),
            _ => None,
        }
    }
}

/// Stable node identifier inside the future property-index tree.
///
/// Invariant:
/// - `0` is reserved as the null node id
/// - live nodes always use non-zero ids
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PropertyIndexNodeId(pub u64);

impl PropertyIndexNodeId {
    /// Null node sentinel.
    pub const NULL: Self = Self(0);

    /// Returns whether this node id is the null sentinel.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// Root metadata for one property-index region.
///
/// This is intentionally similar in spirit to the header used by
/// `ic-stable-structures::BTreeMap`: the root reference, branching factor, and
/// entry count live in a small fixed-width header while node contents live in
/// separately managed storage.
///
/// Invariant:
/// - `entry_count` counts logical indexed bindings, not allocated node slots
/// - `root` may be null only when `entry_count == 0`
/// - `first_leaf` and `last_leaf` are null together
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PropertyIndexHeader {
    pub root: PropertyIndexNodeId,
    pub first_leaf: PropertyIndexNodeId,
    pub last_leaf: PropertyIndexNodeId,
    pub entry_count: u64,
    pub branching_factor: u16,
    pub layout_version: u8,
    pub reserved: u8,
}

impl PropertyIndexHeader {
    /// Fixed stable width of one encoded header.
    pub const ENCODED_LEN: usize = 8 + 8 + 8 + 8 + 2 + 1 + 1;

    /// Current low-level layout version for the rewrite-side property index.
    pub const CURRENT_LAYOUT_VERSION: u8 = 1;

    /// Creates one empty header for the given branching factor.
    pub const fn empty(branching_factor: u16) -> Self {
        Self {
            root: PropertyIndexNodeId::NULL,
            first_leaf: PropertyIndexNodeId::NULL,
            last_leaf: PropertyIndexNodeId::NULL,
            entry_count: 0,
            branching_factor,
            layout_version: Self::CURRENT_LAYOUT_VERSION,
            reserved: 0,
        }
    }

    /// Encodes this header to fixed-width little-endian bytes.
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..8].copy_from_slice(&self.root.0.to_le_bytes());
        out[8..16].copy_from_slice(&self.first_leaf.0.to_le_bytes());
        out[16..24].copy_from_slice(&self.last_leaf.0.to_le_bytes());
        out[24..32].copy_from_slice(&self.entry_count.to_le_bytes());
        out[32..34].copy_from_slice(&self.branching_factor.to_le_bytes());
        out[34] = self.layout_version;
        out[35] = self.reserved;
        out
    }

    /// Decodes one fixed-width header from stable bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(PropertyIndexError::InvalidHeaderLength(bytes.len()));
        }
        let mut root = [0u8; 8];
        root.copy_from_slice(&bytes[0..8]);
        let mut first_leaf = [0u8; 8];
        first_leaf.copy_from_slice(&bytes[8..16]);
        let mut last_leaf = [0u8; 8];
        last_leaf.copy_from_slice(&bytes[16..24]);
        let mut entry_count = [0u8; 8];
        entry_count.copy_from_slice(&bytes[24..32]);
        let mut branching_factor = [0u8; 2];
        branching_factor.copy_from_slice(&bytes[32..34]);
        Ok(Self {
            root: PropertyIndexNodeId(u64::from_le_bytes(root)),
            first_leaf: PropertyIndexNodeId(u64::from_le_bytes(first_leaf)),
            last_leaf: PropertyIndexNodeId(u64::from_le_bytes(last_leaf)),
            entry_count: u64::from_le_bytes(entry_count),
            branching_factor: u16::from_le_bytes(branching_factor),
            layout_version: bytes[34],
            reserved: bytes[35],
        })
    }
}

/// Small allocator header for future property-index node storage.
///
/// This mirrors the role that the allocator metadata plays in
/// `ic-stable-structures::BTreeMap`: root/header metadata stays separate from
/// the allocator state that manages node slots/pages.
///
/// Invariant:
/// - `next_node_id` is monotonic and never reuses ids directly
/// - `free_list_head` is `NULL` when no freed node slots are available
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PropertyIndexAllocatorHeader {
    pub next_node_id: u64,
    pub free_list_head: PropertyIndexNodeId,
    pub page_size_bytes: u32,
    pub reserved: u32,
}

impl PropertyIndexAllocatorHeader {
    /// Fixed stable width of one encoded allocator header.
    pub const ENCODED_LEN: usize = 8 + 8 + 4 + 4;

    /// Creates one empty allocator header.
    pub const fn empty(page_size_bytes: u32) -> Self {
        Self {
            next_node_id: 1,
            free_list_head: PropertyIndexNodeId::NULL,
            page_size_bytes,
            reserved: 0,
        }
    }

    /// Encodes this allocator header to fixed-width bytes.
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..8].copy_from_slice(&self.next_node_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.free_list_head.0.to_le_bytes());
        out[16..20].copy_from_slice(&self.page_size_bytes.to_le_bytes());
        out[20..24].copy_from_slice(&self.reserved.to_le_bytes());
        out
    }

    /// Decodes one allocator header from fixed-width bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(PropertyIndexError::InvalidAllocatorHeaderLength(
                bytes.len(),
            ));
        }
        let mut next_node_id = [0u8; 8];
        next_node_id.copy_from_slice(&bytes[0..8]);
        let mut free_list_head = [0u8; 8];
        free_list_head.copy_from_slice(&bytes[8..16]);
        let mut page_size_bytes = [0u8; 4];
        page_size_bytes.copy_from_slice(&bytes[16..20]);
        let mut reserved = [0u8; 4];
        reserved.copy_from_slice(&bytes[20..24]);
        Ok(Self {
            next_node_id: u64::from_le_bytes(next_node_id),
            free_list_head: PropertyIndexNodeId(u64::from_le_bytes(free_list_head)),
            page_size_bytes: u32::from_le_bytes(page_size_bytes),
            reserved: u32::from_le_bytes(reserved),
        })
    }
}

/// Stable node-kind discriminant for future property-index nodes.
///
/// Invariant:
/// - internal nodes carry routing keys only
/// - leaf nodes carry actual indexed bindings
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyIndexNodeKind {
    Internal = 0,
    Leaf = 1,
}

impl PropertyIndexNodeKind {
    /// Encodes this node kind as one stable byte.
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// Decodes one node kind from one stable byte.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Internal),
            1 => Some(Self::Leaf),
            _ => None,
        }
    }
}

/// Fixed-width header prefix for one persisted property-index node.
///
/// Invariant:
/// - `entry_count` is the logical count of keys stored in the node
/// - `capacity` is meaningful for internal nodes and stores the maximum child fanout
/// - `prev_leaf` and `next_leaf` are meaningful only for leaf nodes
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PropertyIndexNodeHeader {
    pub kind: u8,
    pub reserved: u8,
    pub entry_count: u16,
    pub capacity: u16,
    pub reserved2: u16,
    pub prev_leaf: PropertyIndexNodeId,
    pub next_leaf: PropertyIndexNodeId,
}

impl PropertyIndexNodeHeader {
    /// Fixed stable width of one encoded node header.
    pub const ENCODED_LEN: usize = 1 + 1 + 2 + 2 + 2 + 8 + 8;

    /// Creates one internal-node header.
    pub const fn internal(entry_count: u16) -> Self {
        Self::internal_with_capacity(entry_count, entry_count.saturating_add(1))
    }

    /// Creates one internal-node header with explicit child capacity.
    pub const fn internal_with_capacity(entry_count: u16, capacity: u16) -> Self {
        Self {
            kind: PropertyIndexNodeKind::Internal as u8,
            reserved: 0,
            entry_count,
            capacity,
            reserved2: 0,
            prev_leaf: PropertyIndexNodeId::NULL,
            next_leaf: PropertyIndexNodeId::NULL,
        }
    }

    /// Creates one leaf-node header.
    pub const fn leaf(
        entry_count: u16,
        prev_leaf: PropertyIndexNodeId,
        next_leaf: PropertyIndexNodeId,
    ) -> Self {
        Self {
            kind: PropertyIndexNodeKind::Leaf as u8,
            reserved: 0,
            entry_count,
            capacity: 0,
            reserved2: 0,
            prev_leaf,
            next_leaf,
        }
    }

    /// Returns this header's decoded node kind.
    pub fn node_kind(self) -> Result<PropertyIndexNodeKind, PropertyIndexError> {
        PropertyIndexNodeKind::from_tag(self.kind)
            .ok_or(PropertyIndexError::UnknownNodeKind(self.kind))
    }

    /// Encodes this fixed-width node header to stable bytes.
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0] = self.kind;
        out[1] = self.reserved;
        out[2..4].copy_from_slice(&self.entry_count.to_le_bytes());
        out[4..6].copy_from_slice(&self.capacity.to_le_bytes());
        out[6..8].copy_from_slice(&self.reserved2.to_le_bytes());
        out[8..16].copy_from_slice(&self.prev_leaf.0.to_le_bytes());
        out[16..24].copy_from_slice(&self.next_leaf.0.to_le_bytes());
        out
    }

    /// Decodes one fixed-width node header from stable bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(PropertyIndexError::InvalidNodeHeaderLength(bytes.len()));
        }
        let mut entry_count = [0u8; 2];
        entry_count.copy_from_slice(&bytes[2..4]);
        let mut capacity = [0u8; 2];
        capacity.copy_from_slice(&bytes[4..6]);
        let mut reserved2 = [0u8; 2];
        reserved2.copy_from_slice(&bytes[6..8]);
        let mut prev_leaf = [0u8; 8];
        prev_leaf.copy_from_slice(&bytes[8..16]);
        let mut next_leaf = [0u8; 8];
        next_leaf.copy_from_slice(&bytes[16..24]);
        Ok(Self {
            kind: bytes[0],
            reserved: bytes[1],
            entry_count: u16::from_le_bytes(entry_count),
            capacity: u16::from_le_bytes(capacity),
            reserved2: u16::from_le_bytes(reserved2),
            prev_leaf: PropertyIndexNodeId(u64::from_le_bytes(prev_leaf)),
            next_leaf: PropertyIndexNodeId(u64::from_le_bytes(next_leaf)),
        })
    }
}

/// Persisted node record for the future property-index tree.
///
/// This keeps the node boundary explicit before the bucket-backed allocator and
/// split/merge logic are implemented.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyIndexNodeRecord {
    Internal {
        header: PropertyIndexNodeHeader,
        keys: Vec<PropertyIndexKey>,
        children: Vec<PropertyIndexNodeId>,
    },
    Leaf {
        header: PropertyIndexNodeHeader,
        entries: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
    },
}

impl PropertyIndexNodeRecord {
    /// Encodes one persisted node record.
    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let mut out = Vec::new();
        match self {
            Self::Internal {
                header,
                keys,
                children,
            } => {
                out.extend_from_slice(&header.encode());
                out.extend_from_slice(
                    &u16::try_from(keys.len())
                        .map_err(|_| PropertyIndexError::LengthOverflow)?
                        .to_le_bytes(),
                );
                out.extend_from_slice(
                    &u16::try_from(children.len())
                        .map_err(|_| PropertyIndexError::LengthOverflow)?
                        .to_le_bytes(),
                );
                for key in keys {
                    let key_bytes = key.encode()?;
                    out.extend_from_slice(
                        &u32::try_from(key_bytes.len())
                            .map_err(|_| PropertyIndexError::LengthOverflow)?
                            .to_le_bytes(),
                    );
                    out.extend_from_slice(&key_bytes);
                }
                for child in children {
                    out.extend_from_slice(&child.0.to_le_bytes());
                }
            }
            Self::Leaf { header, entries } => {
                out.extend_from_slice(&header.encode());
                out.extend_from_slice(
                    &u16::try_from(entries.len())
                        .map_err(|_| PropertyIndexError::LengthOverflow)?
                        .to_le_bytes(),
                );
                for (key, entry) in entries {
                    let key_bytes = key.encode()?;
                    let value_bytes = entry.to_bytes();
                    out.extend_from_slice(
                        &u32::try_from(key_bytes.len())
                            .map_err(|_| PropertyIndexError::LengthOverflow)?
                            .to_le_bytes(),
                    );
                    out.extend_from_slice(
                        &u32::try_from(value_bytes.len())
                            .map_err(|_| PropertyIndexError::LengthOverflow)?
                            .to_le_bytes(),
                    );
                    out.extend_from_slice(&key_bytes);
                    out.extend_from_slice(value_bytes.as_ref());
                }
            }
        }
        Ok(out)
    }

    /// Decodes one persisted node record.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() < PropertyIndexNodeHeader::ENCODED_LEN + 2 {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        let header =
            PropertyIndexNodeHeader::decode(&bytes[..PropertyIndexNodeHeader::ENCODED_LEN])?;
        let mut offset = PropertyIndexNodeHeader::ENCODED_LEN;
        match header.node_kind()? {
            PropertyIndexNodeKind::Internal => {
                if bytes.len().saturating_sub(offset) < 4 {
                    return Err(PropertyIndexError::RecordTooShort(
                        bytes.len().saturating_sub(offset),
                    ));
                }
                let mut key_count = [0u8; 2];
                key_count.copy_from_slice(&bytes[offset..offset + 2]);
                let key_count = u16::from_le_bytes(key_count) as usize;
                let mut child_count = [0u8; 2];
                child_count.copy_from_slice(&bytes[offset + 2..offset + 4]);
                let child_count = u16::from_le_bytes(child_count) as usize;
                offset += 4;
                let mut keys = Vec::with_capacity(key_count);
                for _ in 0..key_count {
                    if bytes.len().saturating_sub(offset) < 4 {
                        return Err(PropertyIndexError::RecordTooShort(
                            bytes.len().saturating_sub(offset),
                        ));
                    }
                    let mut key_len = [0u8; 4];
                    key_len.copy_from_slice(&bytes[offset..offset + 4]);
                    let key_len = u32::from_le_bytes(key_len) as usize;
                    offset += 4;
                    let key_end = offset
                        .checked_add(key_len)
                        .ok_or(PropertyIndexError::LengthOverflow)?;
                    if key_end > bytes.len() {
                        return Err(PropertyIndexError::RecordLengthMismatch {
                            expected: key_end,
                            actual: bytes.len(),
                        });
                    }
                    keys.push(PropertyIndexKey::decode(&bytes[offset..key_end])?);
                    offset = key_end;
                }
                let mut children = Vec::with_capacity(child_count);
                for _ in 0..child_count {
                    if bytes.len().saturating_sub(offset) < 8 {
                        return Err(PropertyIndexError::RecordTooShort(
                            bytes.len().saturating_sub(offset),
                        ));
                    }
                    let mut child = [0u8; 8];
                    child.copy_from_slice(&bytes[offset..offset + 8]);
                    children.push(PropertyIndexNodeId(u64::from_le_bytes(child)));
                    offset += 8;
                }
                Ok(Self::Internal {
                    header,
                    keys,
                    children,
                })
            }
            PropertyIndexNodeKind::Leaf => {
                if bytes.len().saturating_sub(offset) < 2 {
                    return Err(PropertyIndexError::RecordTooShort(
                        bytes.len().saturating_sub(offset),
                    ));
                }
                let mut entry_count = [0u8; 2];
                entry_count.copy_from_slice(&bytes[offset..offset + 2]);
                let entry_count = u16::from_le_bytes(entry_count) as usize;
                offset += 2;
                let mut entries = Vec::with_capacity(entry_count);
                for _ in 0..entry_count {
                    if bytes.len().saturating_sub(offset) < 8 {
                        return Err(PropertyIndexError::RecordTooShort(
                            bytes.len().saturating_sub(offset),
                        ));
                    }
                    let mut key_len = [0u8; 4];
                    key_len.copy_from_slice(&bytes[offset..offset + 4]);
                    let mut value_len = [0u8; 4];
                    value_len.copy_from_slice(&bytes[offset + 4..offset + 8]);
                    let key_len = u32::from_le_bytes(key_len) as usize;
                    let value_len = u32::from_le_bytes(value_len) as usize;
                    offset += 8;
                    let key_end = offset
                        .checked_add(key_len)
                        .ok_or(PropertyIndexError::LengthOverflow)?;
                    let value_end = key_end
                        .checked_add(value_len)
                        .ok_or(PropertyIndexError::LengthOverflow)?;
                    if value_end > bytes.len() {
                        return Err(PropertyIndexError::RecordLengthMismatch {
                            expected: value_end,
                            actual: bytes.len(),
                        });
                    }
                    let key = PropertyIndexKey::decode(&bytes[offset..key_end])?;
                    let entry = PropertyIndexEntry::from_bytes(Cow::Owned(
                        bytes[key_end..value_end].to_vec(),
                    ));
                    entries.push((key, entry));
                    offset = value_end;
                }
                Ok(Self::Leaf { header, entries })
            }
        }
    }
}

/// In-memory allocator-side image for persisted property-index nodes.
///
/// This is the metadata-first stepping stone between the current whole-index
/// snapshot and a future bucket-backed node/page allocator. It keeps node ids,
/// allocator header state, and encoded node records separate from the logical
/// `PropertyIndex`.
///
/// **Persisted shape:** incremental insert/remove paths that write leaves back
/// rely on [`PropertyIndexNodeStore::encode_node_page`] (single page per leaf).
/// Pairwise splits, merges, and three-leaf repacks all use that constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyIndexNodeStore {
    pub allocator: PropertyIndexAllocatorHeader,
    pub free_node_ids: Vec<PropertyIndexNodeId>,
    pub nodes: BTreeMap<PropertyIndexNodeId, PropertyIndexNodeRecord>,
}

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

/// Why [`PropertyIndexNodeStore::incremental_leaf_chain_shape`] (or
/// [`PropertyIndexNodeStore::try_incremental_leaf_chain_shape`]) could not build a consistent
/// `(ordered leaf ids, internal ids, fanout)` view.
///
/// Incremental algorithms assume leaf records are linked into one forward `next_leaf` list that
/// visits every persisted leaf node exactly once, and—when internal nodes exist—that this order
/// matches the internal B-tree's left-to-right leaf order.
///
/// **Recovery:** Rebuild from the logical [`PropertyIndex`] with
/// [`PropertyIndexNodeStore::from_index`], or rebuild the leaf chain from a full sorted entry list
/// using the same partitioning helpers as `from_index`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyIndexLeafChainShapeError {
    /// A leaf-only code path was used but an internal node record is still present.
    LeafOnlyStoreContainsInternalNode,
    /// Non-empty leaf-only store, but no sentinel head (`prev_leaf == NULL`) or root could be inferred.
    CannotInferFirstLeafInLeafOnlyStore,
    /// Following `next_leaf` revisited a node before the walk finished.
    NextLeafCycle {
        /// Node id that would be visited twice.
        at: PropertyIndexNodeId,
    },
    /// `next_leaf` pointed at a missing id or a non-leaf record.
    NextLeafNotLeaf { at: PropertyIndexNodeId },
    /// The forward walk ended having visited a different number of nodes than `leaf_node_ids().len()`.
    NextLeafChainLenMismatch { visited: usize, expected: usize },
    /// [`PropertyIndexNodeStore::infer_root_node_id`] found no root while resolving from internals.
    InternalRootMissing,
    /// Descending leftmost children from the inferred internal root did not reach a leaf (cycle or bad child).
    InternalLeftmostLeafUnreachable { root: PropertyIndexNodeId },
}

impl fmt::Display for PropertyIndexLeafChainShapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LeafOnlyStoreContainsInternalNode => write!(
                f,
                "leaf-only incremental shape path saw an internal node record"
            ),
            Self::CannotInferFirstLeafInLeafOnlyStore => {
                write!(f, "could not infer first leaf in a leaf-only node store")
            }
            Self::NextLeafCycle { at } => {
                write!(f, "next_leaf walk cycle at node id {}", at.0)
            }
            Self::NextLeafNotLeaf { at } => write!(
                f,
                "next_leaf pointed at non-leaf or missing node id {}",
                at.0
            ),
            Self::NextLeafChainLenMismatch { visited, expected } => write!(
                f,
                "next_leaf chain visited {visited} leaves but store has {expected} leaf records"
            ),
            Self::InternalRootMissing => write!(f, "could not infer internal B-tree root"),
            Self::InternalLeftmostLeafUnreachable { root } => write!(
                f,
                "leftmost leaf under internal root {} is unreachable (cycle or invalid child link)",
                root.0
            ),
        }
    }
}

impl std::error::Error for PropertyIndexLeafChainShapeError {}

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

impl OrderedThreeLeafWindow {
    fn old_firsts(&self) -> [Option<PropertyIndexKey>; 3] {
        [
            self.e0.first().map(|(k, _)| k.clone()),
            self.e1.first().map(|(k, _)| k.clone()),
            self.e2.first().map(|(k, _)| k.clone()),
        ]
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
        id
    }

    /// Releases one node id back into the free list.
    pub fn free(&mut self, node_id: PropertyIndexNodeId) -> Option<PropertyIndexNodeRecord> {
        let removed = self.nodes.remove(&node_id)?;
        self.free_node_ids.push(node_id);
        self.allocator.free_list_head = node_id;
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
        })
    }

    /// Builds one minimal persisted node-store image from the current logical index.
    ///
    /// The current phase builds one page-aware leaf chain and then stacks
    /// internal routing layers using the logical branching factor.
    pub fn from_index(index: &PropertyIndex, page_size_bytes: u32) -> Self {
        let mut store = Self::new(page_size_bytes);
        if index.entries.is_empty() {
            return store;
        }
        let entries: Vec<_> = index
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        let leaf_chunks = store.partition_entries_into_leaf_chunks(entries);
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
            if let Some(previous_leaf) = leaf_ids.last().copied() {
                if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) =
                    store.get_mut(previous_leaf)
                {
                    header.next_leaf = leaf_id;
                }
            }
            leaf_ids.push(leaf_id);
        }
        let fanout = usize::from(index.header.branching_factor.max(2));
        let _ = store.build_internal_levels_from_leaf_chain(&leaf_ids, fanout);
        store
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
        let Some((leaf_ids, internal_ids, fanout)) = self.incremental_leaf_chain_shape() else {
            return None;
        };
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
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(&key) else {
            return None;
        };
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
    fn repartition_three_leaf_window_from_merged_entries(
        &mut self,
        l0: PropertyIndexNodeId,
        l1: PropertyIndexNodeId,
        l2: PropertyIndexNodeId,
        prev0: PropertyIndexNodeId,
        next2: PropertyIndexNodeId,
        old_firsts: [Option<PropertyIndexKey>; 3],
        merged: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
    ) -> bool {
        let chunks = self.partition_entries_into_leaf_chunks(merged);
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

            if old_firsts[0] != nf0 {
                if let Some(nf) = nf0 {
                    self.propagate_first_key_change(&paths[0], nf);
                }
            }
            if old_firsts[1] != nf1 {
                if let Some(nf) = nf1 {
                    self.propagate_first_key_change(&paths[1], nf);
                }
            }
            if old_firsts[2] != nf2 {
                if let Some(nf) = nf2 {
                    self.propagate_first_key_change(&paths[2], nf);
                }
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
                if old_firsts[0] != nf0 {
                    if let Some(nf) = nf0 {
                        self.propagate_first_key_change(&path0, nf);
                    }
                }
                if old_firsts[1] != nf1 {
                    if let Some(nf) = nf1 {
                        self.propagate_first_key_change(&path1, nf);
                    }
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

    /// When inserting into the middle or tail leaf of a chain, `try_upsert_three_leaf_redistribute`
    /// must still merge **three** consecutive leaves. Walk backward until `leaf` has two
    /// non-null `next_leaf` hops (so `leaf`, `next`, `next.next` are all valid leaf ids).
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
        let mut merged = win.e0;
        merged.extend(win.e1);
        merged.extend(win.e2);
        match merged.binary_search_by(|(k, _)| k.cmp(&key)) {
            Ok(i) => merged[i] = (key, entry),
            Err(i) => merged.insert(i, (key, entry)),
        }
        self.repartition_three_leaf_window_from_merged_entries(
            win.l0, win.l1, win.l2, win.prev0, win.next2, old_firsts, merged,
        )
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
        let mut merged = win.e0;
        merged.extend(win.e1);
        merged.extend(win.e2);
        let Ok(index) = merged.binary_search_by(|(k, _)| k.cmp(key)) else {
            return true;
        };
        merged.remove(index);
        if merged.is_empty() {
            return false;
        }
        self.repartition_three_leaf_window_from_merged_entries(
            win.l0, win.l1, win.l2, win.prev0, win.next2, old_firsts, merged,
        )
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
        {
            if old_first != new_first {
                self.propagate_first_key_change(&right_path, new_first);
            }
        }

        true
    }

    /// Finds a bipartition of `merged_entries` into two **single-page** leaves (same constraint as
    /// [`Self::encode_node_page`] / [`Self::partition_entries_into_leaf_chunks`]) so pairwise
    /// insert redistribution can fall through to three-leaf repacking when no safe split exists.
    fn find_leaf_redistribution_split(
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
        let Some((leaf_ids, internal_ids, fanout)) = self.incremental_leaf_chain_shape() else {
            return None;
        };
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
        let Some((path, leaf_id)) = self.find_path_to_leaf_for_key(key) else {
            return None;
        };
        let Some((leaf_ids, _, _)) = self.incremental_leaf_chain_shape() else {
            return None;
        };
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
                        if let Some(new_first) = self.first_key_for_subtree(next_leaf) {
                            if new_first != next_old_first {
                                self.propagate_first_key_change(&next_path, new_first);
                            }
                        }
                        return Some(PropertyIndexNodeStoreMutationKind::Redistribute);
                    }
                }
            }
        }

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
                        {
                            if prev_old_first != new_prev_first {
                                self.propagate_first_key_change(&prev_path, new_prev_first);
                            }
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

    fn leaf_node_ids(&self) -> Vec<PropertyIndexNodeId> {
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

    fn partition_entries_into_leaf_chunks(
        &self,
        entries: Vec<(PropertyIndexKey, PropertyIndexEntry)>,
    ) -> Vec<Vec<(PropertyIndexKey, PropertyIndexEntry)>> {
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
        chunks
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
        if old_first != new_first {
            if let Some(new_first) = new_first {
                self.propagate_first_key_change(&path, new_first);
            }
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
        let leaf_capacity = self.max_leaf_entry_count(&leaf_ids).max(1);
        let leaf_index = match leaf_ids.iter().position(|existing| *existing == leaf_id) {
            Some(index) => index,
            None => return false,
        };

        let (mut merged_entries, prev_leaf, next_leaf) = match self.get(leaf_id) {
            Some(PropertyIndexNodeRecord::Leaf { header, entries }) => {
                let mut merged = entries.clone();
                match merged.binary_search_by(|(existing, _)| existing.cmp(&key)) {
                    Ok(index) => {
                        merged[index] = (key, entry);
                        return false;
                    }
                    Err(index) => merged.insert(index, (key, entry)),
                }
                (merged, header.prev_leaf, header.next_leaf)
            }
            Some(PropertyIndexNodeRecord::Internal { .. }) | None => return false,
        };

        if merged_entries.len() <= leaf_capacity {
            return false;
        }

        let split_at = merged_entries.len() / 2;
        let right_entries = merged_entries.split_off(split_at);
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
            header.entry_count = u16::try_from(merged_entries.len()).unwrap_or(u16::MAX);
            *entries = merged_entries;
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
        if old_first != new_first {
            if let Some(new_first) = new_first {
                self.propagate_first_key_change(&path, new_first);
            }
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

            if update_index == 0 {
                if let Some(new_first) = self.first_key_for_subtree(node_id) {
                    self.propagate_first_key_change(&path[..depth], new_first);
                }
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
        if parent_depth > 0 && child_count < Self::min_internal_children(capacity) {
            if !self.try_repair_underfull_internal_at_depth(path, parent_depth) {
                return false;
            }
        }
        if let Some(new_first) = self.first_key_for_subtree(parent_id) {
            self.propagate_first_key_change(&path[..parent_depth], new_first);
        }
        true
    }

    fn first_key_for_subtree(&self, node_id: PropertyIndexNodeId) -> Option<PropertyIndexKey> {
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

        let chunks = self.partition_entries_into_leaf_chunks(entries);

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

    fn select_child_for_key(
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
        if let Some(root_id) = self.infer_root_node_id() {
            if let Some(leaf_id) = self.leftmost_leaf_from_root(root_id) {
                return Some(leaf_id);
            }
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
        for page_index in 1..total_pages {
            let remaining = payload.len() - offset;
            let len = overflow_capacity.min(remaining);
            pages[page_index][0..4].copy_from_slice(&Self::NODE_OVERFLOW_PAGE_MAGIC);
            pages[page_index][4] = Self::NODE_OVERFLOW_PAGE_VERSION;
            let next = if page_index + 1 < total_pages {
                (page_index + 1) as u64
            } else {
                0
            };
            pages[page_index][5..13].copy_from_slice(&next.to_le_bytes());
            pages[page_index]
                [Self::NODE_OVERFLOW_PAGE_HEADER_LEN..Self::NODE_OVERFLOW_PAGE_HEADER_LEN + len]
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

    /// Encodes this node store as a fixed-slot paged area.
    ///
    /// Each non-null node id owns exactly one initial page slot in the area.
    /// Additional overflow pages remain embedded in the slot payload for now.
    pub fn encode_paged_area(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let page_size = usize::try_from(self.allocator.page_size_bytes)
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let page_count = self.allocator.next_node_id.saturating_sub(1);
        let mut initial_slots = vec![vec![0u8; page_size]; page_count as usize];
        let mut overflow_slots: Vec<Vec<u8>> = Vec::new();

        for raw_node_id in 1..=page_count {
            let node_id = PropertyIndexNodeId(raw_node_id);
            let Some(node) = self.nodes.get(&node_id) else {
                continue;
            };
            let pages = self.encode_node_pages(node)?;
            let initial_index =
                usize::try_from(raw_node_id - 1).map_err(|_| PropertyIndexError::LengthOverflow)?;
            let mut first_page = pages[0].clone();
            let total_node_pages = pages.len();
            if total_node_pages > 1 {
                let first_overflow_slot = page_count
                    .checked_add(
                        u64::try_from(overflow_slots.len())
                            .map_err(|_| PropertyIndexError::LengthOverflow)?,
                    )
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                first_page[9..17].copy_from_slice(&first_overflow_slot.to_le_bytes());
                for (overflow_idx, page) in pages.into_iter().enumerate().skip(1) {
                    let mut overflow_page = page;
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
            }
            initial_slots[initial_index] = first_page;
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
        for slot in initial_slots {
            out.extend_from_slice(&slot);
        }
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
        })
    }

    fn paged_area_pages_offset(
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

/// Equality-index key for one indexed entity/property/value binding.
///
/// Encoded layout:
///
/// - entity kind: `u8`
/// - property name length: `u16 LE`
/// - value length: `u32 LE`
/// - entity id: `u64 BE`
/// - property name bytes
/// - encoded value bytes
///
/// The entity id comes last so duplicate values remain uniquely ordered within
/// one `(property, encoded_value)` prefix.
///
/// Invariant:
/// - bytewise order must group by property before entity id
/// - `encoded_value` is already the stable representation used for equality
///   comparison inside the index
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyIndexKey {
    pub entity_kind: PropertyIndexEntityKind,
    pub property_name: String,
    pub encoded_value: Vec<u8>,
    pub entity_id: u64,
}

impl PropertyIndexKey {
    /// Width of the fixed key prefix in bytes.
    pub const PREFIX_LEN: usize = 1 + 2 + 4 + 8;

    /// Creates one node equality key.
    pub fn node(node_id: NodeId, property_name: impl AsRef<str>, encoded_value: Vec<u8>) -> Self {
        Self {
            entity_kind: PropertyIndexEntityKind::VertexNode,
            property_name: property_name.as_ref().to_owned(),
            encoded_value,
            entity_id: u64::from(node_id),
        }
    }

    /// Creates one edge equality key.
    pub fn edge(edge_id: EdgeId, property_name: impl AsRef<str>, encoded_value: Vec<u8>) -> Self {
        Self {
            entity_kind: PropertyIndexEntityKind::VertexEdge,
            property_name: property_name.as_ref().to_owned(),
            encoded_value,
            entity_id: edge_id,
        }
    }

    /// Creates the lowest key in one exact `(property, value)` equality range.
    pub fn lower_bound(
        entity_kind: PropertyIndexEntityKind,
        property_name: impl AsRef<str>,
        encoded_value: Vec<u8>,
    ) -> Self {
        Self {
            entity_kind,
            property_name: property_name.as_ref().to_owned(),
            encoded_value,
            entity_id: 0,
        }
    }

    /// Creates the lowest key in one `(entity_kind, property_name)` range.
    pub fn property_lower_bound(
        entity_kind: PropertyIndexEntityKind,
        property_name: impl AsRef<str>,
    ) -> Self {
        Self {
            entity_kind,
            property_name: property_name.as_ref().to_owned(),
            encoded_value: Vec::new(),
            entity_id: 0,
        }
    }

    /// Returns the prefix for all entries of one `(entity_kind, property_name)`.
    pub fn property_prefix(
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
    ) -> Result<Vec<u8>, PropertyIndexError> {
        let property_len =
            u16::try_from(property_name.len()).map_err(|_| PropertyIndexError::LengthOverflow)?;
        let mut out = Vec::with_capacity(Self::PREFIX_LEN + property_name.len());
        out.push(entity_kind.tag());
        out.extend_from_slice(&property_len.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(property_name.as_bytes());
        Ok(out)
    }

    /// Encodes this equality key to stable bytes.
    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let property_len = u16::try_from(self.property_name.len())
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let value_len = u32::try_from(self.encoded_value.len())
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let mut out = Vec::with_capacity(
            Self::PREFIX_LEN + self.property_name.len() + self.encoded_value.len(),
        );
        out.push(self.entity_kind.tag());
        out.extend_from_slice(&property_len.to_le_bytes());
        out.extend_from_slice(&value_len.to_le_bytes());
        out.extend_from_slice(&self.entity_id.to_be_bytes());
        out.extend_from_slice(self.property_name.as_bytes());
        out.extend_from_slice(&self.encoded_value);
        Ok(out)
    }

    /// Decodes one equality key from stable bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() < Self::PREFIX_LEN {
            return Err(PropertyIndexError::InvalidKeyLength(bytes.len()));
        }
        let entity_kind = PropertyIndexEntityKind::from_tag(bytes[0])
            .ok_or(PropertyIndexError::UnknownEntityKind(bytes[0]))?;
        let mut property_len = [0u8; 2];
        property_len.copy_from_slice(&bytes[1..3]);
        let property_len = u16::from_le_bytes(property_len) as usize;
        let mut value_len = [0u8; 4];
        value_len.copy_from_slice(&bytes[3..7]);
        let value_len = u32::from_le_bytes(value_len) as usize;
        let mut entity_id = [0u8; 8];
        entity_id.copy_from_slice(&bytes[7..15]);
        let expected_len = Self::PREFIX_LEN + property_len + value_len;
        if bytes.len() != expected_len {
            return Err(PropertyIndexError::KeyLengthMismatch {
                expected: expected_len,
                actual: bytes.len(),
            });
        }
        let property_start = Self::PREFIX_LEN;
        let property_end = property_start + property_len;
        let value_end = property_end + value_len;
        let property_name = std::str::from_utf8(&bytes[property_start..property_end])
            .map_err(PropertyIndexError::InvalidUtf8)?
            .to_owned();
        Ok(Self {
            entity_kind,
            property_name,
            encoded_value: bytes[property_end..value_end].to_vec(),
            entity_id: u64::from_be_bytes(entity_id),
        })
    }

    /// Returns whether this key matches one `(entity_kind, property_name)` property prefix.
    pub fn matches_property_prefix(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
    ) -> bool {
        self.entity_kind == entity_kind && self.property_name == property_name
    }

    /// Returns whether this key matches one exact `(entity_kind, property_name, encoded_value)` prefix.
    pub fn matches_value_prefix(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
        encoded_value: &[u8],
    ) -> bool {
        self.matches_property_prefix(entity_kind, property_name)
            && self.encoded_value == encoded_value
    }
}

impl Storable for PropertyIndexKey {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode().expect("PropertyIndexKey must encode"))
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode().expect("PropertyIndexKey must encode")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("PropertyIndexKey bytes must decode")
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// One leaf payload for an indexed entity binding.
///
/// The first index can keep this empty or very small because the entity id is
/// already inside the key. This type keeps the payload boundary explicit so a
/// later posting-list or metadata payload can be introduced without changing
/// the key semantics.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyIndexEntry {
    pub payload: Vec<u8>,
}

impl PropertyIndexEntry {
    /// Creates one empty payload entry.
    pub const fn empty() -> Self {
        Self {
            payload: Vec::new(),
        }
    }
}

impl Storable for PropertyIndexEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.payload)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.payload
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self {
            payload: bytes.into_owned(),
        }
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// Minimal ordered in-memory entry set for equality and prefix scans.
///
/// This is not the final tree. It is the simplest ordered model that exercises
/// the key semantics before the bucket-backed internal/leaf implementation is
/// added.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyIndex {
    pub header: PropertyIndexHeader,
    pub entries: BTreeMap<PropertyIndexKey, PropertyIndexEntry>,
}

impl PropertyIndex {
    /// Creates one empty index with the given branching factor.
    pub fn new(branching_factor: u16) -> Self {
        Self {
            header: PropertyIndexHeader::empty(branching_factor),
            entries: BTreeMap::new(),
        }
    }

    /// Inserts or replaces one indexed binding.
    pub fn insert(&mut self, key: PropertyIndexKey, entry: PropertyIndexEntry) {
        let inserted_new = self.entries.insert(key, entry).is_none();
        if inserted_new {
            self.header.entry_count += 1;
        }
    }

    /// Removes one indexed binding.
    pub fn remove(&mut self, key: &PropertyIndexKey) -> Option<PropertyIndexEntry> {
        let removed = self.entries.remove(key);
        if removed.is_some() {
            self.header.entry_count -= 1;
        }
        removed
    }

    /// Returns one exact equality binding by key.
    pub fn get(&self, key: &PropertyIndexKey) -> Option<&PropertyIndexEntry> {
        self.entries.get(key)
    }

    /// Returns all entries matching one `(entity_kind, property_name)` prefix.
    pub fn scan_property_prefix(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
    ) -> Vec<(&PropertyIndexKey, &PropertyIndexEntry)> {
        self.entries
            .iter()
            .filter(|(key, _)| key.matches_property_prefix(entity_kind, property_name))
            .collect()
    }

    /// Returns all entries matching one exact `(property, encoded_value)` equality prefix.
    pub fn scan_value_prefix(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
        encoded_value: &[u8],
    ) -> Vec<(&PropertyIndexKey, &PropertyIndexEntry)> {
        self.entries
            .iter()
            .filter(|(key, _)| key.matches_value_prefix(entity_kind, property_name, encoded_value))
            .collect()
    }

    /// Encodes one whole in-memory index snapshot.
    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(
            &u32::try_from(self.entries.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        for (key, entry) in &self.entries {
            let key_bytes = key.encode()?;
            let entry_bytes = entry.to_bytes();
            out.extend_from_slice(
                &u32::try_from(key_bytes.len())
                    .map_err(|_| PropertyIndexError::LengthOverflow)?
                    .to_le_bytes(),
            );
            out.extend_from_slice(
                &u32::try_from(entry_bytes.len())
                    .map_err(|_| PropertyIndexError::LengthOverflow)?
                    .to_le_bytes(),
            );
            out.extend_from_slice(&key_bytes);
            out.extend_from_slice(entry_bytes.as_ref());
        }
        Ok(out)
    }

    /// Decodes one whole in-memory index snapshot.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() < PropertyIndexHeader::ENCODED_LEN + 4 {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        let header = PropertyIndexHeader::decode(&bytes[..PropertyIndexHeader::ENCODED_LEN])?;
        let mut count = [0u8; 4];
        count.copy_from_slice(
            &bytes[PropertyIndexHeader::ENCODED_LEN..PropertyIndexHeader::ENCODED_LEN + 4],
        );
        let count = u32::from_le_bytes(count) as usize;
        let mut offset = PropertyIndexHeader::ENCODED_LEN + 4;
        let mut entries = BTreeMap::new();

        for _ in 0..count {
            if bytes.len().saturating_sub(offset) < 8 {
                return Err(PropertyIndexError::RecordTooShort(
                    bytes.len().saturating_sub(offset),
                ));
            }
            let mut key_len = [0u8; 4];
            key_len.copy_from_slice(&bytes[offset..offset + 4]);
            let mut value_len = [0u8; 4];
            value_len.copy_from_slice(&bytes[offset + 4..offset + 8]);
            let key_len = u32::from_le_bytes(key_len) as usize;
            let value_len = u32::from_le_bytes(value_len) as usize;
            offset += 8;
            let key_end = offset
                .checked_add(key_len)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let value_end = key_end
                .checked_add(value_len)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            if value_end > bytes.len() {
                return Err(PropertyIndexError::RecordLengthMismatch {
                    expected: value_end,
                    actual: bytes.len(),
                });
            }
            let key = PropertyIndexKey::decode(&bytes[offset..key_end])?;
            let entry =
                PropertyIndexEntry::from_bytes(Cow::Owned(bytes[key_end..value_end].to_vec()));
            entries.insert(key, entry);
            offset = value_end;
        }

        Ok(Self { header, entries })
    }
}

/// Serialized **logical** `(PropertyIndex, PropertyIndex)` pair stored in the PIDX snapshot section.
///
/// This is not the only persisted representation: the property-index region also carries separate
/// paged [`PropertyIndexNodeStore`] areas. **An empty snapshot** (`entry_count == 0` on both
/// sides) does **not** imply there is no index data when those node stores are non-empty — for
/// example after a compact writeback that omits redundant logical bytes. Readers must run
/// [`PropertyIndexStorageImage::normalized`] / [`PropertyIndexStorageImage::reconcile`] (or load
/// through [`crate::RewriteGraphPma::hydrate_from_stable_memory`], which applies the same rules)
/// before treating `snapshot` as authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyIndexSnapshot {
    pub node_index: PropertyIndex,
    pub edge_index: PropertyIndex,
}

impl PropertyIndexSnapshot {
    /// Snapshot magic stored at the beginning of the stable payload.
    pub const MAGIC: [u8; 4] = *b"PIDX";

    /// Current snapshot layout version.
    pub const VERSION: u8 = 1;

    /// Creates one empty snapshot with matching branching factors.
    pub fn empty(branching_factor: u16) -> Self {
        Self {
            node_index: PropertyIndex::new(branching_factor),
            edge_index: PropertyIndex::new(branching_factor),
        }
    }

    /// Encodes one property-index snapshot to stable bytes.
    pub fn encode(&self) -> Result<Vec<u8>, PropertyIndexError> {
        let node_bytes = self.node_index.encode()?;
        let edge_bytes = self.edge_index.encode()?;
        let mut out = Vec::new();
        out.extend_from_slice(&Self::MAGIC);
        out.push(Self::VERSION);
        out.extend_from_slice(
            &u32::try_from(node_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        out.extend_from_slice(
            &u32::try_from(edge_bytes.len())
                .map_err(|_| PropertyIndexError::LengthOverflow)?
                .to_le_bytes(),
        );
        out.extend_from_slice(&node_bytes);
        out.extend_from_slice(&edge_bytes);
        Ok(out)
    }

    /// Decodes one property-index snapshot from stable bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyIndexError> {
        if bytes.len() < 13 {
            return Err(PropertyIndexError::RecordTooShort(bytes.len()));
        }
        if bytes[..4] != Self::MAGIC {
            return Err(PropertyIndexError::InvalidMagic(bytes[..4].to_vec()));
        }
        if bytes[4] != Self::VERSION {
            return Err(PropertyIndexError::UnsupportedVersion(bytes[4]));
        }
        let mut node_len = [0u8; 4];
        node_len.copy_from_slice(&bytes[5..9]);
        let node_len = u32::from_le_bytes(node_len) as usize;
        let mut edge_len = [0u8; 4];
        edge_len.copy_from_slice(&bytes[9..13]);
        let edge_len = u32::from_le_bytes(edge_len) as usize;
        let node_start = 13usize;
        let node_end = node_start
            .checked_add(node_len)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let edge_end = node_end
            .checked_add(edge_len)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        if edge_end != bytes.len() {
            return Err(PropertyIndexError::RecordLengthMismatch {
                expected: edge_end,
                actual: bytes.len(),
            });
        }
        Ok(Self {
            node_index: PropertyIndex::decode(&bytes[node_start..node_end])?,
            edge_index: PropertyIndex::decode(&bytes[node_end..edge_end])?,
        })
    }
}

/// Stable-memory image: PIDX logical snapshot plus paged node-store areas for node/edge indices.
///
/// **Invariant (compact writeback / hydration):** facades may persist an **empty**
/// [`PropertyIndexSnapshot`] while [`PropertyIndexNodeStore`] pages still hold the authoritative
/// tree. Loading bytes therefore **must not** conclude “no property index” from the snapshot
/// section alone. [`Self::normalized`] / [`Self::reconcile`] rebuild `snapshot` from non-empty
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
    pub fn from_indices(snapshot: PropertyIndexSnapshot, page_size_bytes: u32) -> Self {
        let node_store = PropertyIndexNodeStore::from_index(&snapshot.node_index, page_size_bytes);
        let edge_store = PropertyIndexNodeStore::from_index(&snapshot.edge_index, page_size_bytes);
        Self {
            snapshot,
            node_store,
            edge_store,
        }
    }

    /// Builds one storage image from already-decoded section payloads and
    /// normalizes it toward the node-store-primary persisted shape.
    pub fn from_sectioned_parts(
        snapshot: PropertyIndexSnapshot,
        node_store: PropertyIndexNodeStore,
        edge_store: PropertyIndexNodeStore,
        branching_factor: u16,
        page_size_bytes: u32,
    ) -> Self {
        Self {
            snapshot,
            node_store,
            edge_store,
        }
        .normalized(branching_factor, page_size_bytes)
    }

    /// Creates one empty storage image with matching logical/node-store state.
    pub fn empty(branching_factor: u16, page_size_bytes: u32) -> Self {
        Self::from_indices(
            PropertyIndexSnapshot::empty(branching_factor),
            page_size_bytes,
        )
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
    pub fn normalized(mut self, branching_factor: u16, page_size_bytes: u32) -> Self {
        self.rebuild_snapshot_from_node_stores(branching_factor);
        self.reconcile(branching_factor, page_size_bytes);
        self
    }

    /// Reconciles logical and persisted representations after decode or fallback.
    ///
    /// Preference order:
    /// - if the logical snapshot is present, derive missing node stores from it
    /// - if a logical side is empty but its node store has content, rebuild the
    ///   logical side from the node store
    ///
    /// Together with [`Self::rebuild_snapshot_from_node_stores`], this is what makes **hydration**
    /// correct when the on-disk PIDX section is empty but paged stores are not.
    pub fn reconcile(&mut self, branching_factor: u16, page_size_bytes: u32) {
        let node_snapshot_empty = self.snapshot.node_index.header.entry_count == 0;
        let edge_snapshot_empty = self.snapshot.edge_index.header.entry_count == 0;
        let node_store_empty = self.node_store.nodes.is_empty();
        let edge_store_empty = self.edge_store.nodes.is_empty();

        if node_snapshot_empty && !node_store_empty {
            self.snapshot.node_index = self.node_store.to_index(branching_factor);
        } else if !node_snapshot_empty && node_store_empty {
            self.node_store =
                PropertyIndexNodeStore::from_index(&self.snapshot.node_index, page_size_bytes);
        }

        if edge_snapshot_empty && !edge_store_empty {
            self.snapshot.edge_index = self.edge_store.to_index(branching_factor);
        } else if !edge_snapshot_empty && edge_store_empty {
            self.edge_store =
                PropertyIndexNodeStore::from_index(&self.snapshot.edge_index, page_size_bytes);
        }
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

    /// Decodes one full storage image from stable bytes.
    ///
    /// Payloads produced by [`Self::encode`] round-trip bit-for-bit. If bytes came from a **compact**
    /// writer that stores an empty [`PropertyIndexSnapshot`] alongside non-empty node stores, the
    /// decoded `snapshot` remains empty until you call [`Self::normalized`] (or [`Self::reconcile`])
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

/// Error type for rewrite-side property-index skeletons.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyIndexError {
    InvalidRegionHeaderLength(usize),
    InvalidHeaderLength(usize),
    InvalidAllocatorHeaderLength(usize),
    InvalidNodeHeaderLength(usize),
    InvalidKeyLength(usize),
    KeyLengthMismatch {
        expected: usize,
        actual: usize,
    },
    RecordTooShort(usize),
    RecordLengthMismatch {
        expected: usize,
        actual: usize,
    },
    UnknownEntityKind(u8),
    UnknownNodeKind(u8),
    InvalidMagic(Vec<u8>),
    UnsupportedVersion(u8),
    InvalidNodePageLength(usize),
    InvalidNodePageMagic(Vec<u8>),
    UnsupportedNodePageVersion(u8),
    InvalidPagedAreaMagic(Vec<u8>),
    UnsupportedPagedAreaVersion(u8),
    InvalidOverflowPageMagic(Vec<u8>),
    UnsupportedOverflowPageVersion(u8),
    MissingNodeSlot(PropertyIndexNodeId),
    OverflowPersistenceNotYetSupported(PropertyIndexNodeId),
    MissingOverflowPage(usize),
    TruncatedNodeOverflowChain {
        expected_payload_len: usize,
        decoded_payload_len: usize,
    },
    NodePageTooSmall(usize),
    NullNodeId,
    NodeTooLarge {
        encoded_len: usize,
        page_size: usize,
    },
    InvalidUtf8(std::str::Utf8Error),
    LengthOverflow,
    MissingPropertyIndexRegion(RegionKind),
    RegionTooSmall {
        kind: RegionKind,
        required: u64,
        capacity: u64,
    },
    TruncatedBucketChain {
        kind: RegionKind,
        logical_len: usize,
        read: usize,
    },
}

impl fmt::Display for PropertyIndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRegionHeaderLength(len) => {
                write!(f, "invalid property-index region header length: {len}")
            }
            Self::InvalidHeaderLength(len) => {
                write!(f, "invalid property-index header length: {len}")
            }
            Self::InvalidAllocatorHeaderLength(len) => {
                write!(f, "invalid property-index allocator header length: {len}")
            }
            Self::InvalidNodeHeaderLength(len) => {
                write!(f, "invalid property-index node header length: {len}")
            }
            Self::InvalidKeyLength(len) => write!(f, "invalid property-index key length: {len}"),
            Self::KeyLengthMismatch { expected, actual } => write!(
                f,
                "property-index key length mismatch: expected {expected}, got {actual}"
            ),
            Self::RecordTooShort(len) => write!(f, "property-index record too short: {len}"),
            Self::RecordLengthMismatch { expected, actual } => write!(
                f,
                "property-index record length mismatch: expected {expected}, got {actual}"
            ),
            Self::UnknownEntityKind(tag) => {
                write!(f, "unknown property-index entity kind tag: {tag}")
            }
            Self::UnknownNodeKind(tag) => write!(f, "unknown property-index node kind tag: {tag}"),
            Self::InvalidMagic(magic) => write!(f, "invalid property-index magic: {magic:?}"),
            Self::UnsupportedVersion(version) => {
                write!(f, "unsupported property-index snapshot version: {version}")
            }
            Self::InvalidNodePageLength(len) => {
                write!(f, "invalid property-index node page length: {len}")
            }
            Self::InvalidNodePageMagic(magic) => {
                write!(f, "invalid property-index node page magic: {magic:?}")
            }
            Self::UnsupportedNodePageVersion(version) => {
                write!(f, "unsupported property-index node page version: {version}")
            }
            Self::InvalidPagedAreaMagic(magic) => {
                write!(f, "invalid property-index paged area magic: {magic:?}")
            }
            Self::UnsupportedPagedAreaVersion(version) => {
                write!(
                    f,
                    "unsupported property-index paged area version: {version}"
                )
            }
            Self::InvalidOverflowPageMagic(magic) => {
                write!(f, "invalid property-index overflow page magic: {magic:?}")
            }
            Self::UnsupportedOverflowPageVersion(version) => {
                write!(
                    f,
                    "unsupported property-index overflow page version: {version}"
                )
            }
            Self::MissingNodeSlot(node_id) => {
                write!(
                    f,
                    "missing property-index node slot for node id {}",
                    node_id.0
                )
            }
            Self::OverflowPersistenceNotYetSupported(node_id) => write!(
                f,
                "property-index node {} requires overflow-page persistence that is not yet stored in paged areas",
                node_id.0
            ),
            Self::MissingOverflowPage(page_index) => {
                write!(
                    f,
                    "missing property-index overflow page at index {page_index}"
                )
            }
            Self::TruncatedNodeOverflowChain {
                expected_payload_len,
                decoded_payload_len,
            } => write!(
                f,
                "truncated property-index node overflow chain: expected {expected_payload_len} payload bytes, decoded {decoded_payload_len}"
            ),
            Self::NodePageTooSmall(page_size) => {
                write!(
                    f,
                    "property-index node page size is too small: {page_size} bytes"
                )
            }
            Self::NullNodeId => write!(f, "null property-index node id has no page slot"),
            Self::NodeTooLarge {
                encoded_len,
                page_size,
            } => write!(
                f,
                "property-index node record is too large for one page: encoded {encoded_len} bytes, page size {page_size} bytes"
            ),
            Self::InvalidUtf8(err) => write!(f, "invalid UTF-8 in property-index key: {err}"),
            Self::LengthOverflow => write!(f, "property-index length overflow"),
            Self::MissingPropertyIndexRegion(kind) => {
                write!(f, "missing property-index region: {kind:?}")
            }
            Self::RegionTooSmall {
                kind,
                required,
                capacity,
            } => write!(
                f,
                "property-index region too small for {kind:?}: required {required} bytes, capacity {capacity} bytes"
            ),
            Self::TruncatedBucketChain {
                kind,
                logical_len,
                read,
            } => write!(
                f,
                "property-index bucket chain truncated for {kind:?}: logical length {logical_len} bytes, read only {read} bytes"
            ),
        }
    }
}

impl std::error::Error for PropertyIndexError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PropertyIndexPagedAreaMetadata {
    allocator: PropertyIndexAllocatorHeader,
    page_count: usize,
}

/// Reads the fixed-width property-index region header from stable memory.
pub fn read_property_index_region_header_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexRegionHeader, PropertyIndexError> {
    let bytes = read_property_index_region_slice(
        manager,
        memory,
        0,
        PropertyIndexRegionHeader::ENCODED_LEN,
    )?;
    PropertyIndexRegionHeader::decode(&bytes)
}

/// Reads one property-index snapshot from stable memory.
pub fn read_property_index_snapshot_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexSnapshot, PropertyIndexError> {
    let bytes = read_property_index_region_bytes(manager, memory)?;
    PropertyIndexSnapshot::decode(&bytes)
}

/// Writes one property-index snapshot to stable memory.
pub fn write_property_index_snapshot_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    snapshot: &PropertyIndexSnapshot,
) -> Result<(), PropertyIndexError> {
    let encoded = snapshot.encode()?;
    write_property_index_region_bytes(manager, memory, &encoded)
}

/// Reads the logical snapshot section from a sectioned property-index region.
pub fn read_property_index_snapshot_section_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexSnapshot, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let bytes = read_property_index_region_slice(
        manager,
        memory,
        PropertyIndexRegionHeader::ENCODED_LEN,
        header.snapshot_len as usize,
    )?;
    PropertyIndexSnapshot::decode(&bytes)
}

/// Reads the node-property index paged-area section from stable memory.
pub fn read_node_property_index_paged_area_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexNodeStore, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let offset = PropertyIndexRegionHeader::ENCODED_LEN
        .checked_add(header.snapshot_len as usize)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    let bytes =
        read_property_index_region_slice(manager, memory, offset, header.node_store_len as usize)?;
    PropertyIndexNodeStore::decode_paged_area(&bytes)
}

/// Reads the edge-property index paged-area section from stable memory.
pub fn read_edge_property_index_paged_area_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexNodeStore, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let offset = PropertyIndexRegionHeader::ENCODED_LEN
        .checked_add(header.snapshot_len as usize)
        .and_then(|value| value.checked_add(header.node_store_len as usize))
        .ok_or(PropertyIndexError::LengthOverflow)?;
    let bytes =
        read_property_index_region_slice(manager, memory, offset, header.edge_store_len as usize)?;
    PropertyIndexNodeStore::decode_paged_area(&bytes)
}

/// Scans node-property index bindings for one exact equality predicate directly from stable memory.
pub fn scan_node_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_value_prefix_from_stable_memory(
        manager,
        memory,
        true,
        PropertyIndexEntityKind::VertexNode,
        property,
        encoded_value,
    )
}

/// Scans edge-property index bindings for one exact equality predicate directly from stable memory.
pub fn scan_edge_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_value_prefix_from_stable_memory(
        manager,
        memory,
        false,
        PropertyIndexEntityKind::VertexEdge,
        property,
        encoded_value,
    )
}

/// Scans node-property index bindings for one property-name prefix directly from stable memory.
pub fn scan_node_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_property_prefix_from_stable_memory(
        manager,
        memory,
        true,
        PropertyIndexEntityKind::VertexNode,
        property,
    )
}

/// Scans edge-property index bindings for one property-name prefix directly from stable memory.
pub fn scan_edge_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    scan_property_index_property_prefix_from_stable_memory(
        manager,
        memory,
        false,
        PropertyIndexEntityKind::VertexEdge,
        property,
    )
}

/// Reads one node-property index node record directly from stable memory by node id.
pub fn read_node_property_index_node_record_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    read_property_index_node_record_from_stable_memory(manager, memory, true, node_id)
}

/// Reads one edge-property index node record directly from stable memory by node id.
pub fn read_edge_property_index_node_record_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    read_property_index_node_record_from_stable_memory(manager, memory, false, node_id)
}

/// Reads one property-index storage image from stable memory.
pub fn read_property_index_storage_image_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexStorageImage, PropertyIndexError> {
    let bytes = read_property_index_region_bytes(manager, memory)?;
    PropertyIndexStorageImage::decode(&bytes)
}

/// Writes one property-index storage image to stable memory.
pub fn write_property_index_storage_image_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    image: &PropertyIndexStorageImage,
) -> Result<(), PropertyIndexError> {
    let encoded = image.encode()?;
    write_property_index_region_bytes(manager, memory, &encoded)
}

fn ensure_memory_covers(
    memory: &impl Memory,
    last_byte_exclusive: u64,
) -> Result<(), PropertyIndexError> {
    let current_pages = memory.size();
    let current_bytes = current_pages
        .checked_mul(WASM_PAGE_SIZE as u64)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }
    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(WASM_PAGE_SIZE as u64);
    if memory.grow(delta_pages) == -1 {
        return Err(PropertyIndexError::LengthOverflow);
    }
    Ok(())
}

fn read_property_index_region_bytes(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<Vec<u8>, PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let mut bytes = vec![0u8; logical_len];
            if logical_len > 0 {
                memory.read(extent.addr.0, &mut bytes);
            }
            Ok(bytes)
        }
        RegionStorageKind::BucketChain => {
            let chain = manager.bucket_chain(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let mut bytes = vec![0u8; logical_len];
            let mut offset = 0usize;
            let mut cursor = chain.head;
            while !cursor.is_null() && offset < logical_len {
                let header = manager.bucket_header(cursor).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
                let len = bucket_size.min(logical_len - offset);
                memory.read(header.addr.0, &mut bytes[offset..offset + len]);
                offset += len;
                cursor = header.next;
            }
            if offset < logical_len {
                return Err(PropertyIndexError::TruncatedBucketChain {
                    kind: RegionKind::PropertyIndex,
                    logical_len,
                    read: offset,
                });
            }
            Ok(bytes)
        }
    }
}

fn read_property_index_region_slice(
    manager: &RegionManager,
    memory: &impl Memory,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;
    let end = offset
        .checked_add(len)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    if end > logical_len {
        return Err(PropertyIndexError::RecordLengthMismatch {
            expected: end,
            actual: logical_len,
        });
    }

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let mut bytes = vec![0u8; len];
            if len > 0 {
                memory.read(
                    extent
                        .addr
                        .0
                        .checked_add(offset as u64)
                        .ok_or(PropertyIndexError::LengthOverflow)?,
                    &mut bytes,
                );
            }
            Ok(bytes)
        }
        RegionStorageKind::BucketChain => {
            let chain = manager.bucket_chain(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let mut bytes = vec![0u8; len];
            let mut remaining_skip = offset;
            let mut output_offset = 0usize;
            let mut cursor = chain.head;

            while !cursor.is_null() && output_offset < len {
                let header = manager.bucket_header(cursor).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
                if remaining_skip >= bucket_size {
                    remaining_skip -= bucket_size;
                    cursor = header.next;
                    continue;
                }
                let available = bucket_size - remaining_skip;
                let take = available.min(len - output_offset);
                let start_addr = header
                    .addr
                    .0
                    .checked_add(remaining_skip as u64)
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                memory.read(start_addr, &mut bytes[output_offset..output_offset + take]);
                output_offset += take;
                remaining_skip = 0;
                cursor = header.next;
            }

            if output_offset < len {
                return Err(PropertyIndexError::TruncatedBucketChain {
                    kind: RegionKind::PropertyIndex,
                    logical_len: len,
                    read: output_offset,
                });
            }
            Ok(bytes)
        }
    }
}

fn read_property_index_node_record_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    if node_id.is_null() {
        return Err(PropertyIndexError::NullNodeId);
    }
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let allocator = if node_side {
        read_node_property_index_paged_area_from_stable_memory(manager, memory)?.allocator
    } else {
        read_edge_property_index_paged_area_from_stable_memory(manager, memory)?.allocator
    };
    let helper = PropertyIndexNodeStore {
        allocator,
        free_node_ids: Vec::new(),
        nodes: BTreeMap::new(),
    };
    let page_size = usize::try_from(helper.allocator.page_size_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;

    let section_offset = if node_side {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .ok_or(PropertyIndexError::LengthOverflow)?
    } else {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .and_then(|value| value.checked_add(header.node_store_len as usize))
            .ok_or(PropertyIndexError::LengthOverflow)?
    };
    let paged_prefix = read_property_index_region_slice(
        manager,
        memory,
        section_offset,
        PropertyIndexNodeStore::PAGED_AREA_FIXED_HEADER_LEN,
    )?;
    if paged_prefix[..4] != PropertyIndexNodeStore::PAGED_AREA_MAGIC {
        return Err(PropertyIndexError::InvalidPagedAreaMagic(
            paged_prefix[..4].to_vec(),
        ));
    }
    let paged_version = paged_prefix[4];
    let allocator_start = 5;
    let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
    let mut free_count = [0u8; 4];
    free_count.copy_from_slice(&paged_prefix[allocator_end..allocator_end + 4]);
    let free_count = u32::from_le_bytes(free_count) as usize;
    let pages_start = PropertyIndexNodeStore::paged_area_pages_offset(paged_version, free_count)?;
    let page_offset = helper.node_page_offset(node_id)? as usize;
    let initial_page = read_property_index_region_slice(
        manager,
        memory,
        section_offset
            .checked_add(pages_start)
            .ok_or(PropertyIndexError::LengthOverflow)?
            .checked_add(page_offset)
            .ok_or(PropertyIndexError::LengthOverflow)?,
        page_size,
    )?;
    if initial_page.iter().all(|byte| *byte == 0) {
        return Err(PropertyIndexError::MissingNodeSlot(node_id));
    }

    if paged_version == 1 {
        return helper.decode_node_page(&initial_page);
    }
    let mut pages = vec![initial_page];
    let mut next = [0u8; 8];
    next.copy_from_slice(&pages[0][9..17]);
    let mut next_index = u64::from_le_bytes(next);
    while next_index != 0 {
        let global_index =
            usize::try_from(next_index).map_err(|_| PropertyIndexError::LengthOverflow)?;
        let slot_offset = global_index
            .checked_mul(page_size)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let overflow_offset = section_offset
            .checked_add(pages_start)
            .and_then(|value| value.checked_add(slot_offset))
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let overflow_page =
            read_property_index_region_slice(manager, memory, overflow_offset, page_size)?;
        let mut overflow_next = [0u8; 8];
        overflow_next.copy_from_slice(&overflow_page[5..13]);
        next_index = u64::from_le_bytes(overflow_next);
        pages.push(overflow_page);
    }
    helper.decode_node_pages(&pages)
}

fn read_property_index_paged_area_metadata_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
) -> Result<PropertyIndexPagedAreaMetadata, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let section_offset = if node_side {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .ok_or(PropertyIndexError::LengthOverflow)?
    } else {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .and_then(|value| value.checked_add(header.node_store_len as usize))
            .ok_or(PropertyIndexError::LengthOverflow)?
    };
    let paged_prefix = read_property_index_region_slice(
        manager,
        memory,
        section_offset,
        PropertyIndexNodeStore::PAGED_AREA_FIXED_HEADER_LEN,
    )?;
    if paged_prefix[..4] != PropertyIndexNodeStore::PAGED_AREA_MAGIC {
        return Err(PropertyIndexError::InvalidPagedAreaMagic(
            paged_prefix[..4].to_vec(),
        ));
    }
    let paged_version = paged_prefix[4];
    let allocator_start = 5;
    let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
    let allocator =
        PropertyIndexAllocatorHeader::decode(&paged_prefix[allocator_start..allocator_end])?;
    let mut free_count = [0u8; 4];
    free_count.copy_from_slice(&paged_prefix[allocator_end..allocator_end + 4]);
    let free_count = u32::from_le_bytes(free_count) as usize;
    let mut page_count = [0u8; 8];
    page_count.copy_from_slice(&paged_prefix[allocator_end + 4..allocator_end + 12]);
    let page_count = u64::from_le_bytes(page_count) as usize;
    let _pages_start = PropertyIndexNodeStore::paged_area_pages_offset(paged_version, free_count)?;
    Ok(PropertyIndexPagedAreaMetadata {
        allocator,
        page_count,
    })
}

fn scan_property_index_value_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    entity_kind: PropertyIndexEntityKind,
    property: &str,
    encoded_value: &[u8],
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    let target = PropertyIndexKey::lower_bound(entity_kind, property, encoded_value.to_vec());
    let metadata =
        read_property_index_paged_area_metadata_from_stable_memory(manager, memory, node_side)?;
    let Some(mut leaf_id) = find_property_index_leaf_for_key_from_stable_memory(
        manager, memory, node_side, &target, metadata,
    )?
    else {
        return Ok(Vec::new());
    };

    let mut visited = BTreeSet::new();
    let mut out = Vec::new();
    loop {
        if !visited.insert(leaf_id) {
            break;
        }
        let record = read_property_index_node_record_from_stable_memory(
            manager, memory, node_side, leaf_id,
        )?;
        let PropertyIndexNodeRecord::Leaf { header, entries } = record else {
            break;
        };
        let mut saw_matching_prefix = false;
        let mut should_stop = false;
        for (key, entry) in entries {
            if key.matches_value_prefix(entity_kind, property, encoded_value) {
                saw_matching_prefix = true;
                out.push((key, entry));
            } else if saw_matching_prefix || key > target {
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
    Ok(out)
}

fn scan_property_index_property_prefix_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    entity_kind: PropertyIndexEntityKind,
    property: &str,
) -> Result<Vec<(PropertyIndexKey, PropertyIndexEntry)>, PropertyIndexError> {
    let target = PropertyIndexKey::property_lower_bound(entity_kind, property);
    let metadata =
        read_property_index_paged_area_metadata_from_stable_memory(manager, memory, node_side)?;
    let Some(mut leaf_id) = find_property_index_leaf_for_key_from_stable_memory(
        manager, memory, node_side, &target, metadata,
    )?
    else {
        return Ok(Vec::new());
    };

    let mut visited = BTreeSet::new();
    let mut out = Vec::new();
    loop {
        if !visited.insert(leaf_id) {
            break;
        }
        let record = read_property_index_node_record_from_stable_memory(
            manager, memory, node_side, leaf_id,
        )?;
        let PropertyIndexNodeRecord::Leaf { header, entries } = record else {
            break;
        };
        let mut saw_matching_prefix = false;
        let mut should_stop = false;
        for (key, entry) in entries {
            if key.matches_property_prefix(entity_kind, property) {
                saw_matching_prefix = true;
                out.push((key, entry));
            } else if saw_matching_prefix || key > target {
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
    Ok(out)
}

fn find_property_index_leaf_for_key_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    target: &PropertyIndexKey,
    metadata: PropertyIndexPagedAreaMetadata,
) -> Result<Option<PropertyIndexNodeId>, PropertyIndexError> {
    let Some(mut current) =
        infer_property_index_root_id_from_stable_memory(manager, memory, node_side, metadata)?
    else {
        return Ok(None);
    };
    let mut visited = BTreeSet::new();
    loop {
        if !visited.insert(current) {
            return Ok(None);
        }
        let record = read_property_index_node_record_from_stable_memory(
            manager, memory, node_side, current,
        )?;
        match record {
            PropertyIndexNodeRecord::Leaf { .. } => return Ok(Some(current)),
            PropertyIndexNodeRecord::Internal { keys, children, .. } => {
                let child_index =
                    PropertyIndexNodeStore::select_child_for_key(&keys, children.len(), target);
                let Some(next) = children.get(child_index).copied() else {
                    return Ok(None);
                };
                current = next;
            }
        }
    }
}

fn infer_property_index_root_id_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    metadata: PropertyIndexPagedAreaMetadata,
) -> Result<Option<PropertyIndexNodeId>, PropertyIndexError> {
    let mut internal_ids = BTreeSet::new();
    let mut referenced_internal_ids = BTreeSet::new();
    let mut fallback_first_leaf = None;

    for raw in 1..=metadata.page_count {
        let node_id = PropertyIndexNodeId(raw as u64);
        let record = match read_property_index_node_record_from_stable_memory(
            manager, memory, node_side, node_id,
        ) {
            Ok(record) => record,
            Err(PropertyIndexError::MissingNodeSlot(_)) => continue,
            Err(err) => return Err(err),
        };
        match record {
            PropertyIndexNodeRecord::Internal { children, .. } => {
                internal_ids.insert(node_id);
                for child_id in children {
                    referenced_internal_ids.insert(child_id);
                }
            }
            PropertyIndexNodeRecord::Leaf { header, .. } => {
                if fallback_first_leaf.is_none() && header.prev_leaf.is_null() {
                    fallback_first_leaf = Some(node_id);
                }
            }
        }
    }

    if let Some(root_id) = internal_ids
        .iter()
        .find(|node_id| !referenced_internal_ids.contains(node_id))
        .copied()
        .or_else(|| internal_ids.iter().next().copied())
    {
        return Ok(Some(root_id));
    }

    Ok(fallback_first_leaf)
}

fn write_property_index_region_bytes(
    manager: &mut RegionManager,
    memory: &impl Memory,
    encoded: &[u8],
) -> Result<(), PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            manager
                .set_region_logical_len(RegionKind::PropertyIndex, encoded.len() as u64)
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let capacity = usize::try_from(extent.len_bytes)
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            if encoded.len() > capacity {
                return Err(PropertyIndexError::RegionTooSmall {
                    kind: RegionKind::PropertyIndex,
                    required: encoded.len() as u64,
                    capacity: extent.len_bytes,
                });
            }
            ensure_memory_covers(memory, extent.addr.0 + extent.len_bytes)?;
            let mut padded = vec![0u8; capacity];
            padded[..encoded.len()].copy_from_slice(encoded);
            memory.write(extent.addr.0, &padded);
            Ok(())
        }
        RegionStorageKind::BucketChain => {
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let chain = manager
                .ensure_bucket_region_capacity(RegionKind::PropertyIndex, encoded.len() as u64)
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            let required_buckets = encoded.len().max(1).div_ceil(bucket_size);
            let last_byte_exclusive = manager
                .bucket_header(chain.tail)
                .map(|header| header.addr.0 + manager.bucket_size_bytes())
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            ensure_memory_covers(memory, last_byte_exclusive)?;

            let mut cursor = chain.head;
            let mut offset = 0usize;
            let mut written = 0usize;
            while !cursor.is_null() && written < required_buckets {
                let header = manager.bucket_header(cursor).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
                let remaining = encoded.len().saturating_sub(offset);
                let len = bucket_size.min(remaining);
                let mut padded = vec![0u8; bucket_size];
                if len > 0 {
                    padded[..len].copy_from_slice(&encoded[offset..offset + len]);
                    offset += len;
                }
                memory.write(header.addr.0, &padded);
                written += 1;
                cursor = header.next;
            }
            manager
                .set_region_logical_len(RegionKind::PropertyIndex, encoded.len() as u64)
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::low_level::{BucketSizeInPages, RegionManager};
    use crate::property_store::default_property_region_chain;
    use crate::stable::VecMemory;

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

        let store = PropertyIndexNodeStore::from_index(&index, 4096);
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
        let store = PropertyIndexNodeStore::from_index(&index, 192);
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

        let store = PropertyIndexNodeStore::from_index(&index, 192);
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

        let store = PropertyIndexNodeStore::from_index(&index, 192);
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

        let store = PropertyIndexNodeStore::from_index(&index, 4096);
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
        assert_eq!(leaf_counts.iter().copied().map(u16::from).sum::<u16>(), 5);
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
            if let Some(previous) = leaves.last().copied() {
                if let Some(PropertyIndexNodeRecord::Leaf { header, .. }) = store.get_mut(previous)
                {
                    header.next_leaf = leaf;
                }
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
        let mut store = PropertyIndexNodeStore::from_index(&index, 192);

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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index,
            },
            4096,
        );

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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index: node_index.clone(),
                edge_index: edge_index.clone(),
            },
            256,
        );

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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index: PropertyIndex::new(64),
            },
            4096,
        );
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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index: PropertyIndex::new(64),
            },
            128,
        );
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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index: PropertyIndex::new(64),
                edge_index,
            },
            256,
        );
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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index: PropertyIndex::new(64),
                edge_index,
            },
            256,
        );
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
        let image = PropertyIndexStorageImage::from_indices(
            PropertyIndexSnapshot {
                node_index,
                edge_index: PropertyIndex::new(64),
            },
            256,
        );

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
    fn property_index_storage_image_can_reconcile_snapshot_from_node_store() {
        let mut image = PropertyIndexStorageImage::empty(64, 4096);
        let mut index = PropertyIndex::new(64);
        index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "uid", b"alice".to_vec()),
            PropertyIndexEntry::empty(),
        );
        image.node_store = PropertyIndexNodeStore::from_index(&index, 4096);

        image.reconcile(64, 4096);

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
    /// that shape; `normalized` restores logical indices (same contract as facade hydration).
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
        let node_store = PropertyIndexNodeStore::from_index(&node_index, page);
        let edge_store = PropertyIndexNodeStore::from_index(&edge_index, page);

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

        let restored = decoded.normalized(bf, page);
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
        let chunks = store.partition_entries_into_leaf_chunks(entries.clone());
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
        let chunks = store.partition_entries_into_leaf_chunks(entries.clone());
        assert!(
            chunks.len() >= 5,
            "expected at least 5 single-page chunks, got {}",
            chunks.len()
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
                .len(),
            1
        );

        assert!(store.repartition_three_leaf_window_from_merged_entries(
            l0,
            l1,
            l2,
            PropertyIndexNodeId::NULL,
            PropertyIndexNodeId::NULL,
            old_firsts,
            merged,
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
            l0,
            l1,
            l2,
            PropertyIndexNodeId::NULL,
            PropertyIndexNodeId::NULL,
            old_firsts,
            merged,
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
