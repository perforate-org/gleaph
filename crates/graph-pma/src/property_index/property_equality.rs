//! Single stable B-tree backing for the node + edge property equality index (PIDX v3).

use std::cell::RefCell;
use std::rc::Rc;

use ic_stable_structures::{Memory as IcMemory, StableBTreeMap, VectorMemory};

use crate::low_level::{RegionManager, WASM_PAGE_SIZE};
use crate::stable::Memory as StableMemoryTrait;

use super::ic_pidx_linear_memory::PropertyIndexBtreeSubregionIcMemory;
use super::pidx_v3_layout::{PIDX_V3_HEADER_LEN, PropertyIndexRegionHeaderV3};
use super::{
    PropertyIndex, PropertyIndexEntityKind, PropertyIndexEntry, PropertyIndexError,
    PropertyIndexKey, PropertyIndexSnapshot,
};

pub type PropertyEqualityStableMap =
    StableBTreeMap<PropertyIndexKey, PropertyIndexEntry, VectorMemory>;

pub type PropertyEqualityInplaceMap<M> =
    StableBTreeMap<PropertyIndexKey, PropertyIndexEntry, PropertyIndexBtreeSubregionIcMemory<M>>;

pub fn pad_ic_vector_memory_bytes(mut v: Vec<u8>) -> Vec<u8> {
    if v.is_empty() {
        return v;
    }
    let page = WASM_PAGE_SIZE as usize;
    let padded_len = v.len().div_ceil(page) * page;
    v.resize(padded_len, 0);
    v
}

pub fn empty_property_equality_map() -> PropertyEqualityStableMap {
    StableBTreeMap::init(VectorMemory::default())
}

/// Fresh equality map backed by the PIDX v3 btree subregion (logical bytes after the fixed header).
pub fn empty_property_equality_inplace_map<M: StableMemoryTrait>(
    manager: Rc<RefCell<RegionManager>>,
    memory: Rc<RefCell<M>>,
    btree_payload_len: Rc<RefCell<u64>>,
) -> PropertyEqualityInplaceMap<M> {
    StableBTreeMap::init(PropertyIndexBtreeSubregionIcMemory::new(
        manager,
        memory,
        btree_payload_len,
    ))
}

/// Hydrates [`StableBTreeMap::init`] against existing subregion bytes; `btree_payload_len` must match the v3 header.
pub fn hydrate_property_equality_inplace_map<M: StableMemoryTrait>(
    manager: Rc<RefCell<RegionManager>>,
    memory: Rc<RefCell<M>>,
    btree_payload_len: Rc<RefCell<u64>>,
) -> PropertyEqualityInplaceMap<M> {
    StableBTreeMap::init(PropertyIndexBtreeSubregionIcMemory::new(
        manager,
        memory,
        btree_payload_len,
    ))
}

pub fn clone_property_equality_map(src: &PropertyEqualityStableMap) -> PropertyEqualityStableMap {
    let mem = VectorMemory::default();
    let mut dst = StableBTreeMap::init(Rc::clone(&mem));
    for e in src.iter() {
        dst.insert(e.key().clone(), e.value().clone());
    }
    dst
}

pub fn serialize_property_equality_map(map: &PropertyEqualityStableMap) -> Vec<u8> {
    let cloned = clone_property_equality_map(map);
    cloned.into_memory().borrow().clone()
}

pub fn serialize_property_equality_btree<M: IcMemory>(
    map: &StableBTreeMap<PropertyIndexKey, PropertyIndexEntry, M>,
) -> Vec<u8> {
    let mem = VectorMemory::default();
    let mut dst = StableBTreeMap::init(Rc::clone(&mem));
    for e in map.iter() {
        dst.insert(e.key().clone(), e.value().clone());
    }
    dst.into_memory().borrow().clone()
}

pub fn hydrate_property_equality_map_from_serialized_bytes(
    bytes: Vec<u8>,
) -> Result<PropertyEqualityStableMap, PropertyIndexError> {
    let bytes = pad_ic_vector_memory_bytes(bytes);
    let mem = Rc::new(RefCell::new(bytes));
    Ok(StableBTreeMap::init(mem))
}

pub fn encode_pidx_v3_region(
    map: &PropertyEqualityStableMap,
) -> Result<Vec<u8>, PropertyIndexError> {
    let btree_bytes = serialize_property_equality_map(map);
    let header = PropertyIndexRegionHeaderV3 {
        btree_payload_len: btree_bytes.len() as u64,
    };
    let mut out = Vec::with_capacity(PIDX_V3_HEADER_LEN + btree_bytes.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&btree_bytes);
    Ok(out)
}

pub fn decode_pidx_v3_region(
    bytes: &[u8],
) -> Result<PropertyEqualityStableMap, PropertyIndexError> {
    if bytes.len() < PIDX_V3_HEADER_LEN {
        return Err(PropertyIndexError::RecordTooShort(bytes.len()));
    }
    let header = PropertyIndexRegionHeaderV3::decode(&bytes[..PIDX_V3_HEADER_LEN])?;
    let payload_end = PIDX_V3_HEADER_LEN
        .checked_add(
            usize::try_from(header.btree_payload_len)
                .map_err(|_| PropertyIndexError::LengthOverflow)?,
        )
        .ok_or(PropertyIndexError::LengthOverflow)?;
    if payload_end != bytes.len() {
        return Err(PropertyIndexError::RecordLengthMismatch {
            expected: payload_end,
            actual: bytes.len(),
        });
    }
    hydrate_property_equality_map_from_serialized_bytes(
        bytes[PIDX_V3_HEADER_LEN..payload_end].to_vec(),
    )
}

pub fn snapshot_from_equality_any_memory<M: IcMemory>(
    map: &StableBTreeMap<PropertyIndexKey, PropertyIndexEntry, M>,
    branching_factor: u16,
) -> PropertyIndexSnapshot {
    let mut node_index = PropertyIndex::new(branching_factor);
    let mut edge_index = PropertyIndex::new(branching_factor);
    for e in map.iter() {
        let k = e.key().clone();
        let v = e.value().clone();
        match k.entity_kind {
            PropertyIndexEntityKind::VertexNode => node_index.insert(k, v),
            PropertyIndexEntityKind::VertexEdge => edge_index.insert(k, v),
        }
    }
    PropertyIndexSnapshot {
        node_index,
        edge_index,
    }
}

pub fn snapshot_from_equality_map(
    map: &PropertyEqualityStableMap,
    branching_factor: u16,
) -> PropertyIndexSnapshot {
    snapshot_from_equality_any_memory(map, branching_factor)
}

pub fn build_equality_map_from_snapshot(
    snapshot: &PropertyIndexSnapshot,
) -> PropertyEqualityStableMap {
    let mut map = empty_property_equality_map();
    for (k, v) in &snapshot.node_index.entries {
        map.insert(k.clone(), v.clone());
    }
    for (k, v) in &snapshot.edge_index.entries {
        map.insert(k.clone(), v.clone());
    }
    map
}
