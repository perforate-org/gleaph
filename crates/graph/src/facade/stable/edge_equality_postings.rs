//! Stable postings for edge-property equality probes during Expand.

use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{BTreeSet, Memory, Storable};
use ic_stable_structures::storable::Bound;
use std::borrow::Cow;
use std::cmp::Ordering;

const POSTING_KEY_MAGIC: u8 = 3;

/// Lexicographic order: `property_id`, `value`, `owner_vertex_id`, `label_id`, `slot_index`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EdgeEqualityPostingKey {
    pub property_id: u32,
    pub value: Vec<u8>,
    pub owner_vertex_id: u32,
    pub label_id: u16,
    pub slot_index: u32,
}

impl PartialOrd for EdgeEqualityPostingKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EdgeEqualityPostingKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.property_id
            .cmp(&other.property_id)
            .then_with(|| self.value.cmp(&other.value))
            .then_with(|| self.owner_vertex_id.cmp(&other.owner_vertex_id))
            .then_with(|| self.label_id.cmp(&other.label_id))
            .then_with(|| self.slot_index.cmp(&other.slot_index))
    }
}

impl Storable for EdgeEqualityPostingKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.encode())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode(bytes.as_ref()).expect("EdgeEqualityPostingKey decode")
    }
}

impl EdgeEqualityPostingKey {
    pub fn new(
        property_id: PropertyId,
        value_bytes: &[u8],
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) -> Self {
        Self {
            property_id: property_id.raw(),
            value: value_bytes.to_vec(),
            owner_vertex_id: u32::from(owner_vertex_id),
            label_id,
            slot_index,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + 4 + 4 + self.value.len() + 4 + 2 + 4);
        out.push(POSTING_KEY_MAGIC);
        out.extend_from_slice(&self.property_id.to_le_bytes());
        let len_u32: u32 = self
            .value
            .len()
            .try_into()
            .expect("value length must fit u32");
        out.extend_from_slice(&len_u32.to_le_bytes());
        out.extend_from_slice(&self.value);
        out.extend_from_slice(&self.owner_vertex_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out.extend_from_slice(&self.slot_index.to_le_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.first().copied()? != POSTING_KEY_MAGIC {
            return None;
        }
        let property_id = u32::from_le_bytes(bytes.get(1..5)?.try_into().ok()?);
        let vlen = u32::from_le_bytes(bytes.get(5..9)?.try_into().ok()?);
        let usize_len = usize::try_from(vlen).ok()?;
        let val_start: usize = 9;
        let val_end = val_start.checked_add(usize_len)?;
        let value = bytes.get(val_start..val_end)?.to_vec();
        let owner_off = val_end;
        let owner_vertex_id =
            u32::from_le_bytes(bytes.get(owner_off..owner_off + 4)?.try_into().ok()?);
        let label_off = owner_off + 4;
        let label_id = u16::from_le_bytes(bytes.get(label_off..label_off + 2)?.try_into().ok()?);
        let slot_off = label_off + 2;
        let slot_index = u32::from_le_bytes(bytes.get(slot_off..slot_off + 4)?.try_into().ok()?);
        Some(Self {
            property_id,
            value,
            owner_vertex_id,
            label_id,
            slot_index,
        })
    }

    pub fn prefix_lower(property_id: PropertyId, value: &[u8]) -> Self {
        Self {
            property_id: property_id.raw(),
            value: value.to_vec(),
            owner_vertex_id: 0,
            label_id: 0,
            slot_index: 0,
        }
    }

    pub fn prefix_upper(property_id: PropertyId, value: &[u8]) -> Self {
        Self {
            property_id: property_id.raw(),
            value: value.to_vec(),
            owner_vertex_id: u32::MAX,
            label_id: u16::MAX,
            slot_index: u32::MAX,
        }
    }

    pub fn owner_vertex_id(self) -> VertexId {
        VertexId::from(self.owner_vertex_id)
    }

    pub fn property_id(self) -> PropertyId {
        PropertyId::from_raw(self.property_id)
    }
}

pub struct EdgeEqualityPostingStore<M: Memory> {
    postings: BTreeSet<EdgeEqualityPostingKey, M>,
}

impl<M: Memory> EdgeEqualityPostingStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            postings: BTreeSet::init(memory),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.postings.is_empty()
    }

    pub fn insert(&mut self, key: EdgeEqualityPostingKey) {
        self.postings.insert(key);
    }

    pub fn remove(&mut self, key: &EdgeEqualityPostingKey) {
        self.postings.remove(key);
    }

    pub fn lookup_range(
        &self,
        property_id: PropertyId,
        value_bytes: &[u8],
    ) -> Vec<EdgeEqualityPostingKey> {
        let lo = EdgeEqualityPostingKey::prefix_lower(property_id, value_bytes);
        let hi = EdgeEqualityPostingKey::prefix_upper(property_id, value_bytes);
        self.postings
            .range(lo..=hi)
            .filter(|key| key.property_id == property_id.raw() && key.value.as_slice() == value_bytes)
            .collect()
    }

    pub fn into_memory(self) -> M {
        self.postings.into_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::DefaultMemoryImpl;

    #[test]
    fn posting_key_roundtrip() {
        let key = EdgeEqualityPostingKey::new(
            PropertyId::from_raw(7),
            &[1, 2, 3],
            VertexId::from(42),
            3,
            9,
        );
        let bytes = key.encode();
        assert_eq!(EdgeEqualityPostingKey::decode(&bytes).unwrap(), key);
    }

    #[test]
    fn prefix_range_lists_matching_postings() {
        let memory = DefaultMemoryImpl::default();
        let mut store = EdgeEqualityPostingStore::init(memory.clone());
        let pid = PropertyId::from_raw(1);
        let value = vec![5u8];
        store.insert(EdgeEqualityPostingKey::new(
            pid,
            &value,
            VertexId::from(10),
            2,
            0,
        ));
        store.insert(EdgeEqualityPostingKey::new(
            pid,
            &value,
            VertexId::from(11),
            2,
            1,
        ));
        store.insert(EdgeEqualityPostingKey::new(
            pid,
            &[9],
            VertexId::from(12),
            2,
            0,
        ));

        let hits = store.lookup_range(pid, &value);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|key| key.value == value));

        let store2 = EdgeEqualityPostingStore::init(memory);
        assert_eq!(store2.lookup_range(pid, &value).len(), 2);
    }
}
