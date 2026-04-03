use std::borrow::Cow;
use std::collections::BTreeMap;

use gleaph_gql::name_limits;
use gleaph_graph_kernel::{EdgeId, NodeId};

use ic_stable_structures::Storable as IcStorable;
use ic_stable_structures::storable::Bound as IcBound;

use crate::stable::{Bound, Storable};

use super::PropertyIndexError;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyIndexEntityKind {
    VertexNode = b'N',
    VertexEdge = b'E',
}

impl PropertyIndexEntityKind {
    pub const fn tag(self) -> u8 {
        self as u8
    }

    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            b'N' => Some(Self::VertexNode),
            b'E' => Some(Self::VertexEdge),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct PropertyIndexNodeId(pub u64);

impl PropertyIndexNodeId {
    pub const NULL: Self = Self(0);

    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

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
    pub const ENCODED_LEN: usize = 8 + 8 + 8 + 8 + 2 + 1 + 1;
    pub const CURRENT_LAYOUT_VERSION: u8 = 1;

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

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PropertyIndexAllocatorHeader {
    pub next_node_id: u64,
    pub free_list_head: PropertyIndexNodeId,
    pub page_size_bytes: u32,
    pub reserved: u32,
}

impl PropertyIndexAllocatorHeader {
    pub const ENCODED_LEN: usize = 8 + 8 + 4 + 4;

    pub const fn empty(page_size_bytes: u32) -> Self {
        Self {
            next_node_id: 1,
            free_list_head: PropertyIndexNodeId::NULL,
            page_size_bytes,
            reserved: 0,
        }
    }

    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..8].copy_from_slice(&self.next_node_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.free_list_head.0.to_le_bytes());
        out[16..20].copy_from_slice(&self.page_size_bytes.to_le_bytes());
        out[20..24].copy_from_slice(&self.reserved.to_le_bytes());
        out
    }

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

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyIndexNodeKind {
    Internal = 0,
    Leaf = 1,
}

impl PropertyIndexNodeKind {
    pub const fn tag(self) -> u8 {
        self as u8
    }

    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Internal),
            1 => Some(Self::Leaf),
            _ => None,
        }
    }
}

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
    pub const ENCODED_LEN: usize = 1 + 1 + 2 + 2 + 2 + 8 + 8;

    pub const fn internal(entry_count: u16) -> Self {
        Self::internal_with_capacity(entry_count, entry_count.saturating_add(1))
    }

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

    pub fn node_kind(self) -> Result<PropertyIndexNodeKind, PropertyIndexError> {
        PropertyIndexNodeKind::from_tag(self.kind)
            .ok_or(PropertyIndexError::UnknownNodeKind(self.kind))
    }

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
                    let value_bytes = crate::stable::Storable::to_bytes(entry);
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
                    let entry = crate::stable::Storable::from_bytes(Cow::Owned(
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PropertyIndexKey {
    pub entity_kind: PropertyIndexEntityKind,
    pub property_name: String,
    pub encoded_value: Vec<u8>,
    pub entity_id: u64,
}

impl PropertyIndexKey {
    pub const PREFIX_LEN: usize = 1 + 2 + 4 + 8;

    pub fn node(node_id: NodeId, property_name: impl AsRef<str>, encoded_value: Vec<u8>) -> Self {
        Self {
            entity_kind: PropertyIndexEntityKind::VertexNode,
            property_name: property_name.as_ref().to_owned(),
            encoded_value,
            entity_id: u64::from(node_id),
        }
    }

    pub fn edge(edge_id: EdgeId, property_name: impl AsRef<str>, encoded_value: Vec<u8>) -> Self {
        Self {
            entity_kind: PropertyIndexEntityKind::VertexEdge,
            property_name: property_name.as_ref().to_owned(),
            encoded_value,
            entity_id: edge_id,
        }
    }

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

    pub fn matches_property_prefix(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
    ) -> bool {
        self.entity_kind == entity_kind && self.property_name == property_name
    }

    pub fn matches_value_prefix(
        &self,
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
        encoded_value: &[u8],
    ) -> bool {
        self.matches_property_prefix(entity_kind, property_name)
            && crate::byte_compare::eq_u8_slices(&self.encoded_value, encoded_value)
    }

    /// Inclusive start bound for [`StableBTreeMap::range`] when scanning all index entries
    /// for one `(entity_kind, property_name)` (any `encoded_value` / `entity_id`).
    ///
    /// Ordering matches [`PropertyIndexKey`]'s [`Ord`]: `(entity_kind, property_name,
    /// encoded_value, entity_id)`.
    pub fn btree_property_range_start(
        entity_kind: PropertyIndexEntityKind,
        property_name: impl AsRef<str>,
    ) -> Self {
        Self::property_lower_bound(entity_kind, property_name)
    }

    /// Exclusive end bound for that scan when some UTF-8 `property_name` string is strictly
    /// greater than `property_name` under Gleaph's [`name_limits::MAX_PROPERTY_NAME_BYTES`]
    /// cap (see [`name_limits::lexicographic_successor_within_max_bytes`]).
    ///
    /// Use `map.range(start..end)` when `Some(end)`; when `None` (e.g. `property_name` is
    /// already maximal under the successor construction), use `map.range(start..)` and stop at
    /// the first key where [`Self::matches_property_prefix`] fails.
    pub fn btree_property_range_end_exclusive(
        entity_kind: PropertyIndexEntityKind,
        property_name: &str,
    ) -> Option<Self> {
        let next = name_limits::lexicographic_successor_within_max_bytes(
            property_name,
            name_limits::MAX_PROPERTY_NAME_BYTES,
        )?;
        Some(Self::property_lower_bound(entity_kind, next))
    }
}

impl Ord for PropertyIndexKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.entity_kind
            .cmp(&other.entity_kind)
            .then_with(|| self.property_name.cmp(&other.property_name))
            .then_with(|| {
                crate::byte_compare::lex_cmp_u8_slices(&self.encoded_value, &other.encoded_value)
            })
            .then_with(|| self.entity_id.cmp(&other.entity_id))
    }
}

impl PartialOrd for PropertyIndexKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
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

impl IcStorable for PropertyIndexKey {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode().expect("PropertyIndexKey must encode"))
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode().expect("PropertyIndexKey must encode")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("PropertyIndexKey bytes must decode")
    }

    const BOUND: IcBound = IcBound::Unbounded;
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyIndexEntry {
    pub payload: Vec<u8>,
}

impl PropertyIndexEntry {
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

impl IcStorable for PropertyIndexEntry {
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

    const BOUND: IcBound = IcBound::Unbounded;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PropertyIndex {
    pub header: PropertyIndexHeader,
    pub entries: BTreeMap<PropertyIndexKey, PropertyIndexEntry>,
}

impl PropertyIndex {
    pub fn new(branching_factor: u16) -> Self {
        Self {
            header: PropertyIndexHeader::empty(branching_factor),
            entries: BTreeMap::new(),
        }
    }

    pub fn insert(&mut self, key: PropertyIndexKey, entry: PropertyIndexEntry) {
        let inserted_new = self.entries.insert(key, entry).is_none();
        if inserted_new {
            self.header.entry_count += 1;
        }
    }

    pub fn remove(&mut self, key: &PropertyIndexKey) -> Option<PropertyIndexEntry> {
        let removed = self.entries.remove(key);
        if removed.is_some() {
            self.header.entry_count -= 1;
        }
        removed
    }

    pub fn get(&self, key: &PropertyIndexKey) -> Option<&PropertyIndexEntry> {
        self.entries.get(key)
    }

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
            let entry_bytes = crate::stable::Storable::to_bytes(entry);
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
                crate::stable::Storable::from_bytes(Cow::Owned(bytes[key_end..value_end].to_vec()));
            entries.insert(key, entry);
            offset = value_end;
        }

        Ok(Self { header, entries })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyIndexSnapshot {
    pub node_index: PropertyIndex,
    pub edge_index: PropertyIndex,
}

impl PropertyIndexSnapshot {
    pub const MAGIC: [u8; 4] = *b"PIDX";
    pub const VERSION: u8 = 1;

    pub fn empty(branching_factor: u16) -> Self {
        Self {
            node_index: PropertyIndex::new(branching_factor),
            edge_index: PropertyIndex::new(branching_factor),
        }
    }

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
