//! Property-store: stable regions backing node/edge properties.
//!
//! The adjacency kernel keeps hot fixed-size data in PMA-backed regions.
//! Variable-length properties live outside those regions and are intended to be
//! persisted in bucket-backed stable-memory regions.
//!
//! This module defines the first low-level building blocks for that subsystem:
//!
//! - explicit entity/property keys
//! - raw value blobs
//! - **v1 persistence**: a fixed header plus one [`ic_stable_structures::StableBTreeMap`] per region
//!
mod btree_subregion_memory;
mod pstore_v1_layout;

pub(crate) use btree_subregion_memory::PropertyStoreBtreeSubregionIcMemory;
pub use pstore_v1_layout::{
    PROP_STORE_V1_HEADER_LEN, PROP_STORE_V1_MAGIC, PropertyStoreRegionHeaderV1,
};

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::rc::Rc;

use crate::low_level::{
    read_region_logical_slice, write_region_logical_slice, BucketChain, BucketId,
    GleaphMemoryManager, RegionKind, RegionLogicalIoError, RegionManager, RegionStorageKind,
    VirtualBucketMemory, VirtualRegionMemoryError, WASM_PAGE_SIZE,
};
use crate::property_index::PropertyIndexError;
use gleaph_gql::{Value, ValueBinaryError};
use gleaph_graph_kernel::{EdgeId, NodeId};
use ic_stable_structures::StableBTreeMap;
use ic_stable_structures::Storable;
use ic_stable_structures::storable::Bound;
use ic_stable_structures::Memory;

/// Node/edge discriminator for property-store keys.
///
/// Invariant:
/// - node keys and edge keys must never share the same encoded prefix
/// - the encoded discriminant is the first byte of every property key
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PropertyEntityKind {
    Node = b'N',
    Edge = b'E',
}

impl PropertyEntityKind {
    /// Returns the one-byte stable encoding tag for this entity kind.
    pub const fn tag(self) -> u8 {
        self as u8
    }

    /// Decodes one entity kind from its stable one-byte tag.
    pub const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            b'N' => Some(Self::Node),
            b'E' => Some(Self::Edge),
            _ => None,
        }
    }
}

/// Stable-memory key for one node or edge property.
///
/// The encoded bytes are prefix-scan friendly:
///
/// - node property key:
///   - `N | entity_id_be | property_name_bytes`
/// - edge property key:
///   - `E | entity_id_be | property_name_bytes`
///
/// Invariant:
/// - `entity_id` is the stable semantic identity, never a physical locator
/// - `property_name` is stored verbatim after the fixed prefix
/// - all bytes after the fixed prefix belong to the property name
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyKey {
    pub entity_kind: PropertyEntityKind,
    pub entity_id: u64,
    pub property_name: String,
}

impl PropertyKey {
    /// Width of the fixed property-key prefix in bytes.
    pub const PREFIX_LEN: usize = 1 + 8;

    /// Creates one node-property key.
    pub fn node(node_id: NodeId, property_name: impl AsRef<str>) -> Self {
        Self {
            entity_kind: PropertyEntityKind::Node,
            entity_id: u64::from(node_id),
            property_name: property_name.as_ref().to_owned(),
        }
    }

    /// Creates one edge-property key.
    pub fn edge(edge_id: EdgeId, property_name: impl AsRef<str>) -> Self {
        Self {
            entity_kind: PropertyEntityKind::Edge,
            entity_id: edge_id,
            property_name: property_name.as_ref().to_owned(),
        }
    }

    /// Returns the encoded entity prefix used for prefix scans.
    pub fn entity_prefix(
        entity_kind: PropertyEntityKind,
        entity_id: u64,
    ) -> [u8; Self::PREFIX_LEN] {
        let mut out = [0u8; Self::PREFIX_LEN];
        out[0] = entity_kind.tag();
        out[1..].copy_from_slice(&entity_id.to_be_bytes());
        out
    }

    /// Returns the encoded prefix for this key's entity.
    pub fn prefix_bytes(&self) -> [u8; Self::PREFIX_LEN] {
        Self::entity_prefix(self.entity_kind, self.entity_id)
    }

    /// Encodes this key into stable-memory bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::PREFIX_LEN + self.property_name.len());
        out.extend_from_slice(&self.prefix_bytes());
        out.extend_from_slice(self.property_name.as_bytes());
        out
    }

    /// Decodes one key from stable-memory bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyStoreError> {
        if bytes.len() < Self::PREFIX_LEN {
            return Err(PropertyStoreError::InvalidKeyLength(bytes.len()));
        }
        let entity_kind = PropertyEntityKind::from_tag(bytes[0])
            .ok_or(PropertyStoreError::UnknownEntityKind(bytes[0]))?;
        let mut id_bytes = [0u8; 8];
        id_bytes.copy_from_slice(&bytes[1..Self::PREFIX_LEN]);
        let property_name = std::str::from_utf8(&bytes[Self::PREFIX_LEN..])
            .map_err(PropertyStoreError::InvalidUtf8)?
            .to_owned();
        Ok(Self {
            entity_kind,
            entity_id: u64::from_be_bytes(id_bytes),
            property_name,
        })
    }
}

impl Storable for PropertyKey {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("PropertyKey bytes must decode")
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// Opaque property-value payload stored outside the adjacency kernel.
///
/// Invariant:
/// - the property store treats these bytes as the stable source of truth
/// - higher layers define how a runtime `Value` is encoded into this blob
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PropertyValueBlob(pub Vec<u8>);

impl PropertyValueBlob {
    /// Creates one owned property-value blob.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrows the raw payload bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Storable for PropertyValueBlob {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// GQL value as stored in a property-region [`StableBTreeMap`] (newtype for `ic` `Storable`).
#[derive(Clone, Debug, PartialEq)]
pub struct StoredPropertyValue(pub Value);

impl Storable for StoredPropertyValue {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            self.0
                .to_binary_bytes()
                .expect("Value must encode to binary bytes"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
            .to_binary_bytes()
            .expect("Value must encode to binary bytes")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(Value::from_binary_bytes(bytes.as_ref()).expect("Value bytes must decode"))
    }

    const BOUND: Bound = Bound::Unbounded;
}

/// Fixed-width append-log header for one property record.
///
/// Layout:
///
/// - key length: `u32 LE`
/// - value length: `u32 LE`
/// - flags: `u8`
///
/// Invariant:
/// - `value_len == 0` is allowed for both tombstoned and non-tombstoned records
/// - tombstone semantics are carried only by `flags`
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct PropertyRecordHeader {
    pub key_len: u32,
    pub value_len: u32,
    pub flags: u8,
}

impl PropertyRecordHeader {
    /// Stable encoded width of one record header.
    pub const ENCODED_LEN: usize = 9;

    /// Tombstone flag stored inside `flags`.
    pub const FLAG_TOMBSTONE: u8 = 0x01;

    /// Creates one live property-record header.
    pub const fn live(key_len: u32, value_len: u32) -> Self {
        Self {
            key_len,
            value_len,
            flags: 0,
        }
    }

    /// Creates one tombstoned property-record header.
    pub const fn tombstone(key_len: u32) -> Self {
        Self {
            key_len,
            value_len: 0,
            flags: Self::FLAG_TOMBSTONE,
        }
    }

    /// Returns whether this header marks the record as tombstoned.
    pub const fn is_tombstone(self) -> bool {
        (self.flags & Self::FLAG_TOMBSTONE) != 0
    }

    /// Encodes this header to fixed-width bytes.
    pub fn encode(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..4].copy_from_slice(&self.key_len.to_le_bytes());
        out[4..8].copy_from_slice(&self.value_len.to_le_bytes());
        out[8] = self.flags;
        out
    }

    /// Decodes one header from fixed-width bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyStoreError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(PropertyStoreError::InvalidHeaderLength(bytes.len()));
        }
        let mut key_len = [0u8; 4];
        key_len.copy_from_slice(&bytes[0..4]);
        let mut value_len = [0u8; 4];
        value_len.copy_from_slice(&bytes[4..8]);
        Ok(Self {
            key_len: u32::from_le_bytes(key_len),
            value_len: u32::from_le_bytes(value_len),
            flags: bytes[8],
        })
    }
}

/// One append-log property record.
///
/// Invariant:
/// - the header lengths must match the encoded key/value payloads
/// - tombstoned records never carry a value blob
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyRecord<V: Storable> {
    pub header: PropertyRecordHeader,
    pub key: PropertyKey,
    pub value: Option<V>,
}

impl<V: Storable> PropertyRecord<V> {
    /// Creates one live record.
    pub fn live(key: PropertyKey, value: V) -> Result<Self, PropertyStoreError> {
        let value_bytes = value.to_bytes();
        let key_len =
            u32::try_from(key.encode().len()).map_err(|_| PropertyStoreError::LengthOverflow)?;
        let value_len =
            u32::try_from(value_bytes.len()).map_err(|_| PropertyStoreError::LengthOverflow)?;
        Ok(Self {
            header: PropertyRecordHeader::live(key_len, value_len),
            key,
            value: Some(value),
        })
    }

    /// Creates one tombstone record.
    pub fn tombstone(key: PropertyKey) -> Result<Self, PropertyStoreError> {
        let key_len =
            u32::try_from(key.encode().len()).map_err(|_| PropertyStoreError::LengthOverflow)?;
        Ok(Self {
            header: PropertyRecordHeader::tombstone(key_len),
            key,
            value: None,
        })
    }

    /// Returns the total encoded length of this record.
    pub fn encoded_len(&self) -> usize {
        PropertyRecordHeader::ENCODED_LEN
            + self.header.key_len as usize
            + self.header.value_len as usize
    }

    /// Encodes this record as append-log bytes.
    pub fn encode(&self) -> Vec<u8> {
        let key_bytes = self.key.encode();
        let value_bytes: Cow<'_, [u8]> = self
            .value
            .as_ref()
            .map(Storable::to_bytes)
            .unwrap_or_else(|| Cow::Borrowed(&[]));
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.header.encode());
        out.extend_from_slice(&key_bytes);
        out.extend_from_slice(value_bytes.as_ref());
        out
    }

    /// Decodes one record from append-log bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyStoreError> {
        if bytes.len() < PropertyRecordHeader::ENCODED_LEN {
            return Err(PropertyStoreError::RecordTooShort(bytes.len()));
        }
        let header = PropertyRecordHeader::decode(&bytes[..PropertyRecordHeader::ENCODED_LEN])?;
        let expected_len =
            PropertyRecordHeader::ENCODED_LEN + header.key_len as usize + header.value_len as usize;
        if bytes.len() != expected_len {
            return Err(PropertyStoreError::RecordLengthMismatch {
                expected: expected_len,
                actual: bytes.len(),
            });
        }
        let key_start = PropertyRecordHeader::ENCODED_LEN;
        let key_end = key_start + header.key_len as usize;
        let key = PropertyKey::decode(&bytes[key_start..key_end])?;
        let value = if header.is_tombstone() {
            None
        } else {
            Some(V::from_bytes(Cow::Owned(bytes[key_end..].to_vec())))
        };
        Ok(Self { header, key, value })
    }
}

/// Minimal append-log property runtime.
///
/// This is intentionally small. It models the first-phase property-store
/// behavior:
///
/// - append records
/// - tombstone records
/// - rebuild latest-value state
///
/// It does not yet own stable-memory IO or page allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyAppendLog<V: Storable> {
    pub records: Vec<PropertyRecord<V>>,
}

impl<V: Storable> Default for PropertyAppendLog<V> {
    fn default() -> Self {
        Self {
            records: Vec::new(),
        }
    }
}

impl<V: Storable + Clone> PropertyAppendLog<V> {
    /// Encodes the whole append log as one stable-memory payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.records.len() as u32).to_le_bytes());
        for record in &self.records {
            out.extend_from_slice(&record.encode());
        }
        out
    }

    /// Encodes only the appended record suffix `records[start..]`.
    pub fn encode_suffix_from(&self, start: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for record in self.records.get(start..).unwrap_or(&[]) {
            out.extend_from_slice(&record.encode());
        }
        out
    }

    /// Decodes one append log from a stable-memory payload.
    pub fn decode(bytes: &[u8]) -> Result<Self, PropertyStoreError> {
        if bytes.len() < 4 {
            return Err(PropertyStoreError::RecordTooShort(bytes.len()));
        }
        let mut count_bytes = [0u8; 4];
        count_bytes.copy_from_slice(&bytes[..4]);
        let count = u32::from_le_bytes(count_bytes) as usize;
        let mut offset = 4usize;
        let mut records = Vec::with_capacity(count);

        for _ in 0..count {
            if bytes.len().saturating_sub(offset) < PropertyRecordHeader::ENCODED_LEN {
                return Err(PropertyStoreError::RecordTooShort(
                    bytes.len().saturating_sub(offset),
                ));
            }
            let header = PropertyRecordHeader::decode(
                &bytes[offset..offset + PropertyRecordHeader::ENCODED_LEN],
            )?;
            let record_len = PropertyRecordHeader::ENCODED_LEN
                + header.key_len as usize
                + header.value_len as usize;
            let end = offset
                .checked_add(record_len)
                .ok_or(PropertyStoreError::LengthOverflow)?;
            if end > bytes.len() {
                return Err(PropertyStoreError::RecordLengthMismatch {
                    expected: end,
                    actual: bytes.len(),
                });
            }
            records.push(PropertyRecord::decode(&bytes[offset..end])?);
            offset = end;
        }

        Ok(Self { records })
    }

    /// Appends one live record.
    pub fn set(&mut self, key: PropertyKey, value: V) -> Result<(), PropertyStoreError> {
        self.records.push(PropertyRecord::live(key, value)?);
        Ok(())
    }

    /// Appends one tombstone record.
    pub fn remove(&mut self, key: PropertyKey) -> Result<(), PropertyStoreError> {
        self.records.push(PropertyRecord::tombstone(key)?);
        Ok(())
    }

    /// Rebuilds the latest-value state for all keys currently present in the log.
    pub fn latest_state(&self) -> BTreeMap<PropertyKey, Option<V>> {
        let mut out = BTreeMap::new();
        for record in &self.records {
            out.insert(record.key.clone(), record.value.clone());
        }
        out
    }

    /// Returns all latest properties for one entity prefix.
    ///
    /// Scans the append log once for this entity only (does not build a global key map).
    pub fn scan_entity(
        &self,
        entity_kind: PropertyEntityKind,
        entity_id: u64,
    ) -> BTreeMap<String, V> {
        let mut by_key: BTreeMap<PropertyKey, Option<V>> = BTreeMap::new();
        for record in &self.records {
            if record.key.entity_kind != entity_kind || record.key.entity_id != entity_id {
                continue;
            }
            by_key.insert(record.key.clone(), record.value.clone());
        }
        by_key
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| (key.property_name, v)))
            .collect()
    }

    /// Latest properties for many entities in **one** forward scan of the log.
    pub fn scan_entities(
        &self,
        entity_kind: PropertyEntityKind,
        entity_ids: &BTreeSet<u64>,
    ) -> BTreeMap<u64, BTreeMap<String, V>> {
        if entity_ids.is_empty() {
            return BTreeMap::new();
        }
        let mut per_entity: BTreeMap<u64, BTreeMap<PropertyKey, Option<V>>> = BTreeMap::new();
        for record in &self.records {
            if record.key.entity_kind != entity_kind || !entity_ids.contains(&record.key.entity_id)
            {
                continue;
            }
            per_entity
                .entry(record.key.entity_id)
                .or_default()
                .insert(record.key.clone(), record.value.clone());
        }
        let mut out: BTreeMap<u64, BTreeMap<String, V>> = BTreeMap::new();
        for &id in entity_ids {
            let props = per_entity
                .remove(&id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| v.map(|val| (k.property_name, val)))
                .collect();
            out.insert(id, props);
        }
        out
    }

    /// Like [`Self::scan_entities`], but only retains properties whose names appear in
    /// `property_names`. When `property_names` is empty, returns an empty map for each
    /// id without scanning the log.
    pub fn scan_entities_property_subset(
        &self,
        entity_kind: PropertyEntityKind,
        entity_ids: &BTreeSet<u64>,
        property_names: &BTreeSet<String>,
    ) -> BTreeMap<u64, BTreeMap<String, V>> {
        if entity_ids.is_empty() {
            return BTreeMap::new();
        }
        if property_names.is_empty() {
            return entity_ids.iter().map(|&id| (id, BTreeMap::new())).collect();
        }
        let mut per_entity: BTreeMap<u64, BTreeMap<PropertyKey, Option<V>>> = BTreeMap::new();
        for record in &self.records {
            if record.key.entity_kind != entity_kind || !entity_ids.contains(&record.key.entity_id)
            {
                continue;
            }
            if !property_names.contains(&record.key.property_name) {
                continue;
            }
            per_entity
                .entry(record.key.entity_id)
                .or_default()
                .insert(record.key.clone(), record.value.clone());
        }
        let mut out: BTreeMap<u64, BTreeMap<String, V>> = BTreeMap::new();
        for &id in entity_ids {
            let props = per_entity
                .remove(&id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| v.map(|val| (k.property_name, val)))
                .collect();
            out.insert(id, props);
        }
        out
    }

    /// Distinct property names that have a live (non-tombstone) value in this log.
    pub fn distinct_property_names(&self) -> BTreeSet<String> {
        let mut latest: BTreeMap<PropertyKey, bool> = BTreeMap::new();
        for record in &self.records {
            latest.insert(record.key.clone(), record.value.is_some());
        }
        latest
            .into_iter()
            .filter(|(_, alive)| *alive)
            .map(|(k, _)| k.property_name)
            .collect()
    }

    /// Returns the latest value for one exact entity/property key.
    pub fn get_entity_property(
        &self,
        entity_kind: PropertyEntityKind,
        entity_id: u64,
        property_name: &str,
    ) -> Option<V> {
        for record in self.records.iter().rev() {
            if record.key.entity_kind == entity_kind
                && record.key.entity_id == entity_id
                && record.key.property_name == property_name
            {
                return record.value.clone();
            }
        }
        None
    }

    /// Returns the latest node property value for one exact node/property key.
    pub fn get_node_property(&self, node_id: NodeId, property_name: &str) -> Option<V> {
        self.get_entity_property(PropertyEntityKind::Node, u64::from(node_id), property_name)
    }

    /// Returns the latest edge property value for one exact edge/property key.
    pub fn get_edge_property(&self, edge_id: EdgeId, property_name: &str) -> Option<V> {
        self.get_entity_property(PropertyEntityKind::Edge, edge_id, property_name)
    }
}

/// Concrete append-log property runtime using raw value blobs.
pub type BlobPropertyAppendLog = PropertyAppendLog<PropertyValueBlob>;

/// In-memory append-log property runtime using storable GQL values (tests and dedicated write paths).
pub type GraphPropertyAppendLog = PropertyAppendLog<StoredPropertyValue>;

/// Reads one graph-property append log from a fixed-size property region.
pub fn read_graph_property_store_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
) -> Result<GraphPropertyAppendLog, PropertyStoreError> {
    let bytes = read_property_region_bytes(manager, memory, kind)?;
    GraphPropertyAppendLog::decode(&bytes)
}

/// Writes one graph-property append log into a fixed-size property region.
pub fn write_graph_property_store_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    store: &GraphPropertyAppendLog,
) -> Result<(), PropertyStoreError> {
    let encoded = store.encode();
    write_property_region_bytes(manager, memory, kind, &encoded)?;
    Ok(())
}

/// Appends only newly-added records when the persisted property region matches `append_from`.
pub fn write_graph_property_store_suffix_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    store: &GraphPropertyAppendLog,
    append_from: u32,
) -> Result<bool, PropertyStoreError> {
    let region = manager
        .layout
        .region(kind)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
    let old_logical = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyStoreError::LengthOverflow)?;
    if old_logical < 4 {
        return Ok(false);
    }
    let mut header = [0u8; 4];
    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager
                .region_extent(kind)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            memory.read(extent.addr.0, &mut header);
        }
        RegionStorageKind::BucketChain => {
            let chain = manager
                .bucket_chain(kind)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            let bucket = manager
                .bucket_header(chain.head)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            memory.read(bucket.addr.0, &mut header);
        }
    }
    let old_count = u32::from_le_bytes(header);
    let new_count =
        u32::try_from(store.records.len()).map_err(|_| PropertyStoreError::LengthOverflow)?;
    if old_count != append_from || append_from > new_count {
        return Ok(false);
    }
    let suffix = store.encode_suffix_from(append_from as usize);
    let new_logical = old_logical
        .checked_add(suffix.len())
        .ok_or(PropertyStoreError::LengthOverflow)?;
    manager
        .set_region_logical_len(kind, new_logical as u64)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager
                .region_extent(kind)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            ensure_memory_covers(memory, extent.addr.0 + extent.len_bytes)?;
            memory.write(extent.addr.0, &new_count.to_le_bytes());
            if !suffix.is_empty() {
                memory.write(extent.addr.0 + old_logical as u64, &suffix);
            }
        }
        RegionStorageKind::BucketChain => {
            write_property_region_suffix_bytes(
                manager,
                memory,
                kind,
                old_logical,
                &suffix,
                new_logical as u64,
            )?;
            let chain = manager
                .bucket_chain(kind)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            let bucket = manager
                .bucket_header(chain.head)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            memory.write(bucket.addr.0, &new_count.to_le_bytes());
        }
    }
    Ok(true)
}

/// Returns one default empty bucket-backed property-region chain.
pub fn default_property_region_chain() -> BucketChain {
    BucketChain::new(BucketId::NULL, BucketId::NULL, 0)
}

/// Errors from property-store hydrate/write paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyStoreError {
    InvalidKeyLength(usize),
    InvalidHeaderLength(usize),
    RecordTooShort(usize),
    RecordLengthMismatch {
        expected: usize,
        actual: usize,
    },
    UnknownEntityKind(u8),
    InvalidUtf8(std::str::Utf8Error),
    LengthOverflow,
    InvalidBinaryValue(ValueBinaryError),
    MissingPropertyRegion(RegionKind),
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
    /// Property or related identifier rejected by Gleaph name limits.
    InvalidIdentifier(String),
    /// Logical property index could not be synchronized with the persisted node-store layout.
    PropertyIndex(PropertyIndexError),
    PStoreInvalidMagic([u8; 4]),
    PStoreUnsupportedVersion(u8),
    /// Bytes in a property region are neither PSB1 v1 nor a decodable append log.
    PStoreUnsupportedOnDiskLayout,
    /// Region is missing or not bucket-backed when building a [`VirtualBucketMemory`] view.
    VirtualRegionMemory(VirtualRegionMemoryError),
}

impl fmt::Display for PropertyStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKeyLength(len) => write!(f, "invalid property key length: {len}"),
            Self::InvalidHeaderLength(len) => write!(f, "invalid property header length: {len}"),
            Self::RecordTooShort(len) => write!(f, "property record too short: {len}"),
            Self::RecordLengthMismatch { expected, actual } => {
                write!(
                    f,
                    "property record length mismatch: expected {expected}, got {actual}"
                )
            }
            Self::UnknownEntityKind(tag) => write!(f, "unknown property entity kind tag: {tag}"),
            Self::InvalidUtf8(err) => write!(f, "invalid UTF-8 in property key: {err}"),
            Self::LengthOverflow => write!(f, "property record length overflow"),
            Self::InvalidBinaryValue(err) => write!(f, "invalid binary property value: {err}"),
            Self::MissingPropertyRegion(kind) => write!(f, "missing property region: {kind:?}"),
            Self::RegionTooSmall {
                kind,
                required,
                capacity,
            } => write!(
                f,
                "property region too small for {kind:?}: required {required} bytes, capacity {capacity} bytes"
            ),
            Self::TruncatedBucketChain {
                kind,
                logical_len,
                read,
            } => write!(
                f,
                "property bucket chain truncated for {kind:?}: logical length {logical_len} bytes, read only {read} bytes"
            ),
            Self::InvalidIdentifier(msg) => write!(f, "invalid identifier: {msg}"),
            Self::PropertyIndex(err) => write!(f, "property index error: {err}"),
            Self::PStoreInvalidMagic(m) => write!(f, "property store v1 invalid magic: {m:?}"),
            Self::PStoreUnsupportedVersion(v) => {
                write!(f, "property store v1 unsupported version byte: {v}")
            }
            Self::PStoreUnsupportedOnDiskLayout => {
                write!(
                    f,
                    "property store region uses an unsupported on-disk layout"
                )
            }
            Self::VirtualRegionMemory(err) => write!(f, "{err}"),
        }
    }
}

impl From<VirtualRegionMemoryError> for PropertyStoreError {
    fn from(value: VirtualRegionMemoryError) -> Self {
        Self::VirtualRegionMemory(value)
    }
}

impl Error for PropertyStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8(err) => Some(err),
            Self::InvalidBinaryValue(err) => Some(err),
            Self::PropertyIndex(err) => Some(err),
            Self::VirtualRegionMemory(err) => Some(err),
            _ => None,
        }
    }
}

impl From<PropertyIndexError> for PropertyStoreError {
    fn from(value: PropertyIndexError) -> Self {
        Self::PropertyIndex(value)
    }
}

impl From<ValueBinaryError> for PropertyStoreError {
    fn from(value: ValueBinaryError) -> Self {
        Self::InvalidBinaryValue(value)
    }
}

impl From<RegionLogicalIoError> for PropertyStoreError {
    fn from(value: RegionLogicalIoError) -> Self {
        match value {
            RegionLogicalIoError::MissingRegion(kind) => Self::MissingPropertyRegion(kind),
            RegionLogicalIoError::LengthOverflow => Self::LengthOverflow,
            RegionLogicalIoError::RecordLengthMismatch { expected, actual } => {
                Self::RecordLengthMismatch { expected, actual }
            }
            RegionLogicalIoError::RegionTooSmall {
                kind,
                required,
                capacity,
            } => Self::RegionTooSmall {
                kind,
                required,
                capacity,
            },
            RegionLogicalIoError::TruncatedBucketChain {
                kind,
                logical_len,
                read,
            } => Self::TruncatedBucketChain {
                kind,
                logical_len,
                read,
            },
        }
    }
}

fn ensure_memory_covers(
    memory: &impl Memory,
    last_byte_exclusive: u64,
) -> Result<(), PropertyStoreError> {
    let current_pages = memory.size();
    let current_bytes = current_pages
        .checked_mul(WASM_PAGE_SIZE)
        .ok_or(PropertyStoreError::LengthOverflow)?;
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }
    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(WASM_PAGE_SIZE);
    if memory.grow(delta_pages) == -1 {
        return Err(PropertyStoreError::LengthOverflow);
    }
    Ok(())
}

pub(crate) fn read_property_store_region_slice(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, PropertyStoreError> {
    read_region_logical_slice(manager, memory, kind, offset, len).map_err(Into::into)
}

pub(crate) fn write_property_store_region_logical_slice(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    offset: usize,
    bytes: &[u8],
) -> Result<(), PropertyStoreError> {
    write_region_logical_slice(manager, memory, kind, offset, bytes).map_err(Into::into)
}

fn read_property_region_bytes(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
) -> Result<Vec<u8>, PropertyStoreError> {
    let region = manager
        .layout
        .region(kind)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyStoreError::LengthOverflow)?;
    read_region_logical_slice(manager, memory, kind, 0, logical_len).map_err(Into::into)
}

fn write_property_region_bytes(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    encoded: &[u8],
) -> Result<(), PropertyStoreError> {
    let region = manager
        .layout
        .region(kind)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let old_logical = usize::try_from(region.logical_len_bytes)
                .map_err(|_| PropertyStoreError::LengthOverflow)?;
            manager
                .set_region_logical_len(kind, encoded.len() as u64)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            let extent = manager
                .region_extent(kind)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            let capacity = usize::try_from(extent.len_bytes)
                .map_err(|_| PropertyStoreError::LengthOverflow)?;
            if encoded.len() > capacity {
                return Err(PropertyStoreError::RegionTooSmall {
                    kind,
                    required: encoded.len() as u64,
                    capacity: extent.len_bytes,
                });
            }
            ensure_memory_covers(memory, extent.addr.0 + extent.len_bytes)?;
            if !encoded.is_empty() {
                memory.write(extent.addr.0, encoded);
            }
            if old_logical > encoded.len() {
                let clear_len = old_logical - encoded.len();
                const ZMAX: usize = 4096;
                let zero_chunk = [0u8; ZMAX];
                let mut remaining = clear_len;
                let mut pos = extent
                    .addr
                    .0
                    .checked_add(encoded.len() as u64)
                    .ok_or(PropertyStoreError::LengthOverflow)?;
                while remaining > 0 {
                    let take = remaining.min(ZMAX);
                    memory.write(pos, &zero_chunk[..take]);
                    pos = pos
                        .checked_add(take as u64)
                        .ok_or(PropertyStoreError::LengthOverflow)?;
                    remaining -= take;
                }
                crate::bench_profile::record_stat(
                    "property_extent_shrink_cleared_bytes",
                    clear_len as u64,
                );
            }
            crate::bench_profile::record_stat(
                "property_extent_payload_write_bytes",
                encoded.len() as u64,
            );
            Ok(())
        }
        RegionStorageKind::BucketChain => {
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyStoreError::LengthOverflow)?;
            let chain = manager
                .ensure_bucket_region_capacity(kind, encoded.len() as u64)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            let required_buckets = encoded.len().max(1).div_ceil(bucket_size);
            let last_byte_exclusive = manager
                .bucket_header(chain.tail)
                .map(|header| header.addr.0 + manager.bucket_size_bytes())
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            ensure_memory_covers(memory, last_byte_exclusive)?;

            let mut cursor = chain.head;
            let mut offset = 0usize;
            let mut written = 0usize;
            while !cursor.is_null() && written < required_buckets {
                let header = manager
                    .bucket_header(cursor)
                    .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
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
            Ok(())
        }
    }
}

fn write_property_region_suffix_bytes(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    start_offset: usize,
    encoded: &[u8],
    new_logical_len: u64,
) -> Result<(), PropertyStoreError> {
    let region = manager
        .layout
        .region(kind)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager
                .region_extent(kind)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            ensure_memory_covers(memory, extent.addr.0 + extent.len_bytes)?;
            if !encoded.is_empty() {
                memory.write(
                    extent
                        .addr
                        .0
                        .checked_add(start_offset as u64)
                        .ok_or(PropertyStoreError::LengthOverflow)?,
                    encoded,
                );
            }
        }
        RegionStorageKind::BucketChain => {
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyStoreError::LengthOverflow)?;
            let chain = manager
                .ensure_bucket_region_capacity(kind, new_logical_len)
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            let last_byte_exclusive = manager
                .bucket_header(chain.tail)
                .map(|header| header.addr.0 + manager.bucket_size_bytes())
                .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
            ensure_memory_covers(memory, last_byte_exclusive)?;

            let mut cursor = chain.head;
            let mut remaining_skip = start_offset;
            let mut written = 0usize;
            while !cursor.is_null() && written < encoded.len() {
                let header = manager
                    .bucket_header(cursor)
                    .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
                if remaining_skip >= bucket_size {
                    remaining_skip -= bucket_size;
                    cursor = header.next;
                    continue;
                }
                let available = bucket_size - remaining_skip;
                let take = available.min(encoded.len() - written);
                memory.write(
                    header
                        .addr
                        .0
                        .checked_add(remaining_skip as u64)
                        .ok_or(PropertyStoreError::LengthOverflow)?,
                    &encoded[written..written + take],
                );
                written += take;
                remaining_skip = 0;
                cursor = header.next;
            }
            if written < encoded.len() {
                return Err(PropertyStoreError::TruncatedBucketChain {
                    kind,
                    logical_len: new_logical_len as usize,
                    read: start_offset + written,
                });
            }
        }
    }
    Ok(())
}

/// Node or edge property bag backed by stable memory (`PSB1` header + btree).
///
/// `M` is the canister-wide backing memory; btree I/O uses [`VirtualBucketMemory`] for the property region.
pub type GraphPropertyStableMap<M> = StableBTreeMap<
    PropertyKey,
    StoredPropertyValue,
    PropertyStoreBtreeSubregionIcMemory<VirtualBucketMemory<M>>,
>;

pub fn empty_graph_property_stable_map<M: Memory>(
    gleaph: &GleaphMemoryManager<M>,
    btree_payload_len: Rc<RefCell<u64>>,
    region_kind: RegionKind,
) -> Result<GraphPropertyStableMap<M>, VirtualRegionMemoryError> {
    debug_assert!(matches!(
        region_kind,
        RegionKind::NodePropertyStore | RegionKind::EdgePropertyStore
    ));
    let region_memory = gleaph.get_bucket(region_kind)?;
    Ok(GraphPropertyStableMap::init(PropertyStoreBtreeSubregionIcMemory::new(
        Rc::clone(gleaph.manager()),
        region_memory,
        btree_payload_len,
        region_kind,
    )))
}

/// Reads existing btree bytes after [`PROP_STORE_V1_HEADER_LEN`]; `btree_payload_len` must match the header.
pub fn hydrate_graph_property_stable_map<M: Memory>(
    gleaph: &GleaphMemoryManager<M>,
    btree_payload_len: Rc<RefCell<u64>>,
    region_kind: RegionKind,
) -> Result<GraphPropertyStableMap<M>, VirtualRegionMemoryError> {
    empty_graph_property_stable_map(gleaph, btree_payload_len, region_kind)
}

pub fn read_prop_store_v1_header_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
) -> Result<Option<PropertyStoreRegionHeaderV1>, PropertyStoreError> {
    let region = manager
        .layout
        .region(kind)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
    if region.logical_len_bytes < PROP_STORE_V1_HEADER_LEN as u64 {
        return Ok(None);
    }
    let bytes =
        read_property_store_region_slice(manager, memory, kind, 0, PROP_STORE_V1_HEADER_LEN)?;
    let mut m = [0u8; 4];
    m.copy_from_slice(&bytes[0..4]);
    if m != PROP_STORE_V1_MAGIC {
        return Ok(None);
    }
    Ok(Some(PropertyStoreRegionHeaderV1::decode(&bytes)?))
}

pub fn sync_graph_property_store_v1_header_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    btree_payload_len: u64,
) -> Result<(), PropertyStoreError> {
    let header = PropertyStoreRegionHeaderV1 { btree_payload_len };
    write_property_store_region_logical_slice(manager, memory, kind, 0, &header.encode())?;
    let total = (PROP_STORE_V1_HEADER_LEN as u64)
        .checked_add(btree_payload_len)
        .ok_or(PropertyStoreError::LengthOverflow)?;
    manager
        .set_region_logical_len(kind, total)
        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
    Ok(())
}

pub fn write_graph_property_stable_map_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    btree_payload_len: &RefCell<u64>,
    must_flush: bool,
) -> Result<(), PropertyStoreError> {
    if !must_flush {
        return Ok(());
    }
    let len = *btree_payload_len.borrow();
    sync_graph_property_store_v1_header_to_stable_memory(manager, memory, kind, len)?;
    Ok(())
}

fn pstore_region_entity_kind(kind: RegionKind) -> Result<PropertyEntityKind, PropertyStoreError> {
    match kind {
        RegionKind::NodePropertyStore => Ok(PropertyEntityKind::Node),
        RegionKind::EdgePropertyStore => Ok(PropertyEntityKind::Edge),
        _ => Err(PropertyStoreError::MissingPropertyRegion(kind)),
    }
}

/// Loads v1 btree state from stable memory (PSB1 header required when the region is non-empty).
pub fn load_graph_property_stable_map_from_stable_memory<M: Memory>(
    mgr_rc: Rc<RefCell<RegionManager>>,
    mem_rc: Rc<M>,
    kind: RegionKind,
) -> Result<(GraphPropertyStableMap<M>, Rc<RefCell<u64>>), PropertyStoreError> {
    pstore_region_entity_kind(kind)?;
    let gleaph = GleaphMemoryManager::new(Rc::clone(&mgr_rc), Rc::clone(&mem_rc));
    let bytes = {
        let mgr = mgr_rc.borrow();
        read_property_region_bytes(&mgr, mem_rc.as_ref(), kind)?
    };

    let btree_rc = Rc::new(RefCell::new(0u64));

    if bytes.is_empty() {
        let map = empty_graph_property_stable_map(
            &gleaph,
            Rc::clone(&btree_rc),
            kind,
        )?;
        return Ok((map, btree_rc));
    }

    if bytes.len() >= PROP_STORE_V1_HEADER_LEN {
        let mut m = [0u8; 4];
        m.copy_from_slice(&bytes[0..4]);
        if m == PROP_STORE_V1_MAGIC {
            let header = PropertyStoreRegionHeaderV1::decode(&bytes[..PROP_STORE_V1_HEADER_LEN])?;
            let raw_pl = header.btree_payload_len;
            let virt_pl = raw_pl
                .div_ceil(WASM_PAGE_SIZE)
                .saturating_mul(WASM_PAGE_SIZE);
            *btree_rc.borrow_mut() = virt_pl;
            {
                let mut mgr = mgr_rc.borrow_mut();
                let base = PROP_STORE_V1_HEADER_LEN as u64;
                let min_logical = base.saturating_add(virt_pl);
                let cur_logical = mgr
                    .layout
                    .region(kind)
                    .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?
                    .logical_len_bytes;
                if cur_logical < min_logical {
                    mgr.set_region_logical_len(kind, min_logical)
                        .ok_or(PropertyStoreError::MissingPropertyRegion(kind))?;
                }
                if virt_pl > raw_pl {
                    let gap_offset = usize::try_from(base.saturating_add(raw_pl))
                        .map_err(|_| PropertyStoreError::LengthOverflow)?;
                    let gap_len = usize::try_from(virt_pl.saturating_sub(raw_pl))
                        .map_err(|_| PropertyStoreError::LengthOverflow)?;
                    if gap_len > 0 {
                        let zeros = vec![0u8; gap_len];
                        let m = mem_rc.as_ref();
                        write_property_store_region_logical_slice(
                            &mut mgr, m, kind, gap_offset, &zeros,
                        )?;
                    }
                }
            }
            let map = hydrate_graph_property_stable_map(
                &gleaph,
                Rc::clone(&btree_rc),
                kind,
            )?;
            return Ok((map, btree_rc));
        }
    }

    Err(PropertyStoreError::PStoreUnsupportedOnDiskLayout)
}

pub(crate) fn btree_get_node_property<M: Memory>(
    map: &GraphPropertyStableMap<M>,
    node_id: NodeId,
    property_name: &str,
) -> Option<Value> {
    map.get(&PropertyKey::node(node_id, property_name))
        .map(|w| w.0.clone())
}

pub(crate) fn btree_get_edge_property<M: Memory>(
    map: &GraphPropertyStableMap<M>,
    edge_id: EdgeId,
    property_name: &str,
) -> Option<Value> {
    map.get(&PropertyKey::edge(edge_id, property_name))
        .map(|w| w.0.clone())
}

pub(crate) fn btree_scan_entity<M: Memory>(
    map: &GraphPropertyStableMap<M>,
    entity_kind: PropertyEntityKind,
    entity_id: u64,
) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for e in map.iter() {
        let k = e.key();
        if k.entity_kind == entity_kind && k.entity_id == entity_id {
            out.insert(k.property_name.clone(), e.value().0.clone());
        }
    }
    out
}

pub(crate) fn btree_scan_entities<M: Memory>(
    map: &GraphPropertyStableMap<M>,
    entity_kind: PropertyEntityKind,
    entity_ids: &BTreeSet<u64>,
) -> BTreeMap<u64, BTreeMap<String, Value>> {
    if entity_ids.is_empty() {
        return BTreeMap::new();
    }
    let mut per: BTreeMap<u64, BTreeMap<String, Value>> = BTreeMap::new();
    for e in map.iter() {
        let k = e.key();
        if k.entity_kind != entity_kind || !entity_ids.contains(&k.entity_id) {
            continue;
        }
        per.entry(k.entity_id)
            .or_default()
            .insert(k.property_name.clone(), e.value().0.clone());
    }
    entity_ids
        .iter()
        .map(|&id| (id, per.remove(&id).unwrap_or_default()))
        .collect()
}

pub(crate) fn btree_scan_entities_property_subset<M: Memory>(
    map: &GraphPropertyStableMap<M>,
    entity_kind: PropertyEntityKind,
    entity_ids: &BTreeSet<u64>,
    property_names: &BTreeSet<String>,
) -> BTreeMap<u64, BTreeMap<String, Value>> {
    if entity_ids.is_empty() {
        return BTreeMap::new();
    }
    if property_names.is_empty() {
        return entity_ids.iter().map(|&id| (id, BTreeMap::new())).collect();
    }
    let mut per: BTreeMap<u64, BTreeMap<String, Value>> = BTreeMap::new();
    for e in map.iter() {
        let k = e.key();
        if k.entity_kind != entity_kind
            || !entity_ids.contains(&k.entity_id)
            || !property_names.contains(&k.property_name)
        {
            continue;
        }
        per.entry(k.entity_id)
            .or_default()
            .insert(k.property_name.clone(), e.value().0.clone());
    }
    entity_ids
        .iter()
        .map(|&id| (id, per.remove(&id).unwrap_or_default()))
        .collect()
}

pub(crate) fn btree_distinct_property_names<M: Memory>(
    map: &GraphPropertyStableMap<M>,
) -> BTreeSet<String> {
    map.iter().map(|e| e.key().property_name.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::low_level::{BucketSizeInPages, RegionManager};
    use crate::VecMemory;

    #[test]
    fn property_key_round_trips_through_storable_bytes() {
        let key = PropertyKey::node(NodeId::from(42u8), "uid");
        let encoded = ic_stable_structures::Storable::to_bytes(&key);
        let restored = <PropertyKey as ic_stable_structures::Storable>::from_bytes(encoded);
        assert_eq!(restored, key);
    }

    #[test]
    fn property_key_prefix_matches_entity_identity() {
        let key = PropertyKey::edge(77, "weight");
        assert_eq!(
            key.prefix_bytes(),
            PropertyKey::entity_prefix(PropertyEntityKind::Edge, 77)
        );
    }

    #[test]
    fn property_record_header_encodes_fixed_width_format() {
        let header = PropertyRecordHeader::tombstone(11);
        let decoded = PropertyRecordHeader::decode(&header.encode()).expect("decode header");
        assert_eq!(decoded, header);
        assert!(decoded.is_tombstone());
    }

    #[test]
    fn property_record_round_trips_live_payload() {
        let record = PropertyRecord::<PropertyValueBlob>::live(
            PropertyKey::node(NodeId::from(7u8), "name"),
            PropertyValueBlob::new(vec![1, 2, 3]),
        )
        .expect("live record");
        let restored =
            PropertyRecord::<PropertyValueBlob>::decode(&record.encode()).expect("decode record");
        assert_eq!(restored, record);
    }

    #[test]
    fn property_append_log_rebuilds_latest_state() {
        let key = PropertyKey::node(NodeId::from(9u8), "uid");
        let mut log = BlobPropertyAppendLog::default();
        log.set(key.clone(), PropertyValueBlob::new(vec![1]))
            .expect("set");
        log.set(key.clone(), PropertyValueBlob::new(vec![2]))
            .expect("overwrite");
        let state = log.latest_state();
        assert_eq!(
            state.get(&key),
            Some(&Some(PropertyValueBlob::new(vec![2])))
        );
    }

    #[test]
    fn property_append_log_filters_scan_by_entity() {
        let mut log = BlobPropertyAppendLog::default();
        log.set(
            PropertyKey::node(NodeId::from(1u8), "uid"),
            PropertyValueBlob::new(vec![1]),
        )
        .expect("node prop");
        log.set(
            PropertyKey::edge(11, "weight"),
            PropertyValueBlob::new(vec![9]),
        )
        .expect("edge prop");

        let node_props = log.scan_entity(PropertyEntityKind::Node, 1);
        assert_eq!(
            node_props.get("uid"),
            Some(&PropertyValueBlob::new(vec![1]))
        );
        assert!(!node_props.contains_key("weight"));
    }

    #[test]
    fn scan_entity_agrees_with_latest_state_for_one_entity() {
        let mut log = GraphPropertyAppendLog::default();
        log.set(
            PropertyKey::node(NodeId::from(1u8), "a"),
            StoredPropertyValue(Value::Int64(1)),
        )
        .unwrap();
        log.set(
            PropertyKey::node(NodeId::from(2u8), "b"),
            StoredPropertyValue(Value::Int64(2)),
        )
        .unwrap();
        let expected: BTreeMap<String, StoredPropertyValue> = log
            .latest_state()
            .into_iter()
            .filter_map(|(k, v)| {
                if k.entity_kind == PropertyEntityKind::Node && k.entity_id == 1 {
                    v.map(|val| (k.property_name, val))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(log.scan_entity(PropertyEntityKind::Node, 1), expected);
    }

    #[test]
    fn get_node_property_uses_last_record_for_key() {
        let mut log = GraphPropertyAppendLog::default();
        let k = PropertyKey::node(NodeId::from(3u8), "x");
        log.set(k.clone(), StoredPropertyValue(Value::Int64(1)))
            .unwrap();
        log.set(k.clone(), StoredPropertyValue(Value::Int64(2)))
            .unwrap();
        assert_eq!(
            log.get_node_property(NodeId::from(3u8), "x"),
            Some(StoredPropertyValue(Value::Int64(2)))
        );
        log.remove(k).unwrap();
        assert_eq!(log.get_node_property(NodeId::from(3u8), "x"), None);
        log.set(
            PropertyKey::node(NodeId::from(3u8), "x"),
            StoredPropertyValue(Value::Int64(3)),
        )
        .unwrap();
        assert_eq!(
            log.get_node_property(NodeId::from(3u8), "x"),
            Some(StoredPropertyValue(Value::Int64(3)))
        );
    }

    #[test]
    fn scan_entities_batch_matches_individual_scan_entity() {
        let mut log = GraphPropertyAppendLog::default();
        log.set(
            PropertyKey::node(NodeId::from(1u8), "a"),
            StoredPropertyValue(Value::Int64(1)),
        )
        .unwrap();
        log.set(
            PropertyKey::node(NodeId::from(2u8), "b"),
            StoredPropertyValue(Value::Int64(2)),
        )
        .unwrap();
        let ids: BTreeSet<u64> = [1u64, 2].into_iter().collect();
        let batch = log.scan_entities(PropertyEntityKind::Node, &ids);
        assert_eq!(
            batch.get(&1).unwrap(),
            &log.scan_entity(PropertyEntityKind::Node, 1)
        );
        assert_eq!(
            batch.get(&2).unwrap(),
            &log.scan_entity(PropertyEntityKind::Node, 2)
        );
    }

    #[test]
    fn scan_entities_property_subset_filters_keys_and_skips_empty_filter_without_scan() {
        let mut log = GraphPropertyAppendLog::default();
        log.set(
            PropertyKey::node(NodeId::from(1u8), "a"),
            StoredPropertyValue(Value::Int64(1)),
        )
        .unwrap();
        log.set(
            PropertyKey::node(NodeId::from(1u8), "b"),
            StoredPropertyValue(Value::Int64(2)),
        )
        .unwrap();
        let ids: BTreeSet<u64> = [1u64].into_iter().collect();
        let mut want = BTreeSet::new();
        want.insert("a".to_owned());
        let sub = log.scan_entities_property_subset(PropertyEntityKind::Node, &ids, &want);
        let one = sub.get(&1).unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(
            one.get("a"),
            Some(&StoredPropertyValue(Value::Int64(1)))
        );
        assert!(!one.contains_key("b"));

        let empty: BTreeSet<String> = BTreeSet::new();
        let no_scan = log.scan_entities_property_subset(PropertyEntityKind::Node, &ids, &empty);
        assert_eq!(no_scan.get(&1).unwrap(), &BTreeMap::new());
    }

    #[test]
    fn distinct_property_names_omits_tombstoned_keys() {
        let mut log = GraphPropertyAppendLog::default();
        let k = PropertyKey::node(NodeId::from(1u8), "gone");
        log.set(k.clone(), StoredPropertyValue(Value::Bool(true)))
            .unwrap();
        log.remove(k).unwrap();
        log.set(
            PropertyKey::node(NodeId::from(1u8), "kept"),
            StoredPropertyValue(Value::Bool(false)),
        )
        .unwrap();
        let names = log.distinct_property_names();
        assert!(names.contains("kept"));
        assert!(!names.contains("gone"));
    }

    #[test]
    fn gql_value_round_trips_through_storable_boundary() {
        let value = Value::Record(vec![
            ("uid".to_owned(), Value::Text("u1".to_owned())),
            ("weight".to_owned(), Value::Int64(5)),
        ]);
        let wrapped = StoredPropertyValue(value.clone());
        let restored = StoredPropertyValue::from_bytes(wrapped.to_bytes());
        assert_eq!(restored.0, value);
    }

    #[test]
    fn property_append_log_can_read_exact_node_property() {
        let mut log = GraphPropertyAppendLog::default();
        log.set(
            PropertyKey::node(NodeId::from(7u8), "uid"),
            StoredPropertyValue(Value::Text("u7".into())),
        )
        .expect("set property");

        assert_eq!(
            log.get_node_property(NodeId::from(7u8), "uid"),
            Some(StoredPropertyValue(Value::Text("u7".into())))
        );
        assert_eq!(log.get_node_property(NodeId::from(7u8), "missing"), None);
    }

    #[test]
    fn graph_property_store_round_trips_through_bucket_backed_region() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            default_property_region_chain(),
        );

        let mut store = GraphPropertyAppendLog::default();
        let large_text = "x".repeat((WASM_PAGE_SIZE as usize) + 1024);
        store
            .set(
                PropertyKey::node(NodeId::from(42u8), "payload"),
                StoredPropertyValue(Value::Text(large_text.clone())),
            )
            .expect("set large property");

        write_graph_property_store_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::NodePropertyStore,
            &store,
        )
        .expect("write property store");

        let restored = read_graph_property_store_from_stable_memory(
            &manager,
            &memory,
            RegionKind::NodePropertyStore,
        )
        .expect("read property store");

        assert_eq!(
            restored.get_node_property(NodeId::from(42u8), "payload"),
            Some(StoredPropertyValue(Value::Text(large_text)))
        );
        let chain = manager
            .bucket_chain(RegionKind::NodePropertyStore)
            .expect("bucket chain");
        assert_ne!(
            chain.head, chain.tail,
            "large payload should span multiple buckets"
        );
    }

    #[test]
    fn graph_property_store_shorter_payload_round_trips_logical_length() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            default_property_region_chain(),
        );

        let mut initial = GraphPropertyAppendLog::default();
        initial
            .set(
                PropertyKey::node(NodeId::from(5u8), "payload"),
                StoredPropertyValue(Value::Text(
                    "x".repeat((WASM_PAGE_SIZE as usize) + 2048),
                )),
            )
            .expect("set initial");
        write_graph_property_store_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::NodePropertyStore,
            &initial,
        )
        .expect("write initial");

        let mut rewritten = GraphPropertyAppendLog::default();
        rewritten
            .set(
                PropertyKey::node(NodeId::from(5u8), "payload"),
                StoredPropertyValue(Value::Text("short".into())),
            )
            .expect("set rewritten");
        write_graph_property_store_to_stable_memory(
            &mut manager,
            &memory,
            RegionKind::NodePropertyStore,
            &rewritten,
        )
        .expect("write rewritten");

        let restored = read_graph_property_store_from_stable_memory(
            &manager,
            &memory,
            RegionKind::NodePropertyStore,
        )
        .expect("read rewritten");
        assert_eq!(
            restored.get_node_property(NodeId::from(5u8), "payload"),
            Some(StoredPropertyValue(Value::Text("short".into())))
        );
    }

    #[test]
    fn reading_truncated_bucket_chain_returns_error() {
        let memory = VecMemory::default();
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            default_property_region_chain(),
        );
        assert_eq!(memory.grow(1), 0);
        manager
            .set_region_logical_len(RegionKind::NodePropertyStore, WASM_PAGE_SIZE + 1)
            .expect("set logical len");

        let err = read_graph_property_store_from_stable_memory(
            &manager,
            &memory,
            RegionKind::NodePropertyStore,
        )
        .expect_err("truncated chain should fail");
        assert!(matches!(
            err,
            PropertyStoreError::TruncatedBucketChain {
                kind: RegionKind::NodePropertyStore,
                ..
            }
        ));
    }

    #[test]
    fn graph_property_stable_map_large_value_round_trips() {
        let mem_rc = Rc::new(VecMemory::default());
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::NodePropertyStore,
            default_property_region_chain(),
        );
        let mgr_rc = Rc::new(RefCell::new(manager));
        let gleaph = GleaphMemoryManager::new(Rc::clone(&mgr_rc), Rc::clone(&mem_rc));
        let btree_rc = Rc::new(RefCell::new(0u64));
        let mut map = empty_graph_property_stable_map(
            &gleaph,
            Rc::clone(&btree_rc),
            RegionKind::NodePropertyStore,
        )
        .expect("node property bucket region");
        let large = Value::Text("x".repeat((WASM_PAGE_SIZE as usize) + 512));
        let _ = map.insert(
            PropertyKey::node(NodeId::from(11u8), "profile"),
            StoredPropertyValue(large.clone()),
        );
        sync_graph_property_store_v1_header_to_stable_memory(
            &mut mgr_rc.borrow_mut(),
            mem_rc.as_ref(),
            RegionKind::NodePropertyStore,
            *btree_rc.borrow(),
        )
        .expect("sync header");

        let (map2, _) = load_graph_property_stable_map_from_stable_memory(
            Rc::clone(&mgr_rc),
            Rc::clone(&mem_rc),
            RegionKind::NodePropertyStore,
        )
        .expect("reload");
        assert_eq!(
            btree_get_node_property(&map2, NodeId::from(11u8), "profile"),
            Some(large)
        );
    }
}
