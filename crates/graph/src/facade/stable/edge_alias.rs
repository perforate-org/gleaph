use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeAliasKey {
    alias_vertex_id: u32,
    label_id: u16,
    alias_slot_index: u32,
}

impl EdgeAliasKey {
    pub fn new(alias_vertex_id: VertexId, label_id: u16, alias_slot_index: u32) -> Self {
        Self {
            alias_vertex_id: u32::from(alias_vertex_id),
            label_id,
            alias_slot_index,
        }
    }

    pub fn label_id(self) -> u16 {
        self.label_id
    }

    pub fn alias_vertex_id(self) -> VertexId {
        VertexId::from(self.alias_vertex_id)
    }

    pub fn alias_slot_key(self) -> u32 {
        self.alias_slot_index
    }
}

impl Storable for EdgeAliasKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 10,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.extend_from_slice(&self.alias_vertex_id.to_be_bytes());
        out.extend_from_slice(&self.label_id.to_be_bytes());
        out.extend_from_slice(&self.alias_slot_index.to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 10, "EdgeAliasKey expects exactly 10 bytes");

        let mut vertex = [0; 4];
        let mut label = [0; 2];
        let mut slot = [0; 4];
        vertex.copy_from_slice(&bytes[0..4]);
        label.copy_from_slice(&bytes[4..6]);
        slot.copy_from_slice(&bytes[6..10]);

        Self {
            alias_vertex_id: u32::from_be_bytes(vertex),
            label_id: u16::from_be_bytes(label),
            alias_slot_index: u32::from_be_bytes(slot),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeAliasValue {
    canonical_vertex_id: u32,
    canonical_slot_index: u32,
}

impl EdgeAliasValue {
    pub fn new(canonical_vertex_id: VertexId, canonical_slot_index: u32) -> Self {
        Self {
            canonical_vertex_id: u32::from(canonical_vertex_id),
            canonical_slot_index,
        }
    }

    pub fn canonical_vertex_id(self) -> VertexId {
        VertexId::from(self.canonical_vertex_id)
    }

    pub fn canonical_slot_index(self) -> u32 {
        self.canonical_slot_index
    }
}

impl Storable for EdgeAliasValue {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&self.canonical_vertex_id.to_be_bytes());
        out.extend_from_slice(&self.canonical_slot_index.to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 8, "EdgeAliasValue expects exactly 8 bytes");

        let mut vertex = [0; 4];
        let mut slot = [0; 4];
        vertex.copy_from_slice(&bytes[0..4]);
        slot.copy_from_slice(&bytes[4..8]);

        Self {
            canonical_vertex_id: u32::from_be_bytes(vertex),
            canonical_slot_index: u32::from_be_bytes(slot),
        }
    }
}

pub struct EdgeAliasIndex<M: Memory> {
    aliases: StableBTreeMap<EdgeAliasKey, EdgeAliasValue, M>,
}

/// Raw serialized key/value bytes occupied by one alias entry, excluding
/// StableBTreeMap node and allocator overhead.
pub const EDGE_ALIAS_KEY_BYTES: u64 = 10;
pub const EDGE_ALIAS_VALUE_BYTES: u64 = 8;
pub const EDGE_ALIAS_RAW_ENTRY_BYTES: u64 = EDGE_ALIAS_KEY_BYTES + EDGE_ALIAS_VALUE_BYTES;

impl<M: Memory> EdgeAliasIndex<M> {
    pub fn init(memory: M) -> Self {
        Self {
            aliases: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(
        &mut self,
        alias_vertex_id: VertexId,
        label_id: u16,
        alias_slot_index: u32,
        canonical_vertex_id: VertexId,
        canonical_slot_index: u32,
    ) -> Option<EdgeAliasValue> {
        self.aliases.insert(
            EdgeAliasKey::new(alias_vertex_id, label_id, alias_slot_index),
            EdgeAliasValue::new(canonical_vertex_id, canonical_slot_index),
        )
    }

    pub fn get(
        &self,
        alias_vertex_id: VertexId,
        label_id: u16,
        alias_slot_index: u32,
    ) -> Option<EdgeAliasValue> {
        self.aliases.get(&EdgeAliasKey::new(
            alias_vertex_id,
            label_id,
            alias_slot_index,
        ))
    }

    pub fn remove(
        &mut self,
        alias_vertex_id: VertexId,
        label_id: u16,
        alias_slot_index: u32,
    ) -> Option<EdgeAliasValue> {
        self.aliases.remove(&EdgeAliasKey::new(
            alias_vertex_id,
            label_id,
            alias_slot_index,
        ))
    }

    pub fn len(&self) -> u64 {
        self.aliases.len()
    }

    pub fn is_empty(&self) -> bool {
        self.aliases.is_empty()
    }

    /// Returns the raw serialized key/value payload size, excluding B-tree
    /// node and memory-manager allocation overhead.
    pub fn raw_payload_bytes(&self) -> u64 {
        self.len().saturating_mul(EDGE_ALIAS_RAW_ENTRY_BYTES)
    }

    pub fn move_alias_key(
        &mut self,
        alias_vertex_id: VertexId,
        label_id: u16,
        old_alias_slot_index: u32,
        new_alias_slot_index: u32,
    ) -> Option<EdgeAliasValue> {
        if old_alias_slot_index == new_alias_slot_index {
            return self.get(alias_vertex_id, label_id, old_alias_slot_index);
        }
        let value = self.remove(alias_vertex_id, label_id, old_alias_slot_index)?;
        self.aliases.insert(
            EdgeAliasKey::new(alias_vertex_id, label_id, new_alias_slot_index),
            value,
        );
        Some(value)
    }

    pub fn move_canonical_target(
        &mut self,
        canonical_vertex_id: VertexId,
        label_id: u16,
        old_canonical_slot_index: u32,
        new_canonical_slot_index: u32,
    ) -> usize {
        if old_canonical_slot_index == new_canonical_slot_index {
            return 0;
        }
        let keys: Vec<_> = self
            .aliases
            .iter()
            .filter_map(|entry| {
                let (key, value) = entry.into_pair();
                (key.label_id() == label_id
                    && value.canonical_vertex_id() == canonical_vertex_id
                    && value.canonical_slot_index() == old_canonical_slot_index)
                    .then_some(key)
            })
            .collect();
        let moved = keys.len();
        for key in keys {
            self.aliases.insert(
                key,
                EdgeAliasValue::new(canonical_vertex_id, new_canonical_slot_index),
            );
        }
        moved
    }

    pub fn remove_all_for_canonical(
        &mut self,
        canonical_vertex_id: VertexId,
        label_id: u16,
        canonical_slot_index: u32,
    ) -> usize {
        let keys: Vec<_> = self
            .aliases
            .iter()
            .filter_map(|entry| {
                let (key, value) = entry.into_pair();
                (key.label_id() == label_id
                    && value.canonical_vertex_id() == canonical_vertex_id
                    && value.canonical_slot_index() == canonical_slot_index)
                    .then_some(key)
            })
            .collect();
        let removed = keys.len();
        for key in keys {
            self.aliases.remove(&key);
        }
        removed
    }

    pub(crate) fn for_each<F>(&self, mut f: F)
    where
        F: FnMut(EdgeAliasKey, EdgeAliasValue),
    {
        for entry in self.aliases.iter() {
            let (key, value) = entry.into_pair();
            f(key, value);
        }
    }

    pub(crate) fn clear_all(&mut self) {
        let keys: Vec<_> = self.aliases.iter().map(|entry| *entry.key()).collect();
        for key in keys {
            self.aliases.remove(&key);
        }
    }

    pub fn find_alias_for_canonical(
        &self,
        canonical_vertex_id: VertexId,
        label_id: u16,
        canonical_slot_index: u32,
    ) -> Option<(VertexId, u32)> {
        self.aliases.iter().find_map(|entry| {
            let (key, value) = entry.into_pair();
            (key.label_id() == label_id
                && value.canonical_vertex_id() == canonical_vertex_id
                && value.canonical_slot_index() == canonical_slot_index)
                .then_some((VertexId::from(key.alias_vertex_id), key.alias_slot_index))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    #[test]
    fn stores_and_removes_aliases() {
        let mut index = EdgeAliasIndex::init(VectorMemory::default());
        let alias = VertexId::from(2);
        let canonical = VertexId::from(1);

        index.insert(alias, 7, 3, canonical, 9);

        let value = index.get(alias, 7, 3).expect("alias");
        assert_eq!(value.canonical_vertex_id(), canonical);
        assert_eq!(value.canonical_slot_index(), 9);

        assert_eq!(index.remove_all_for_canonical(canonical, 7, 9), 1);
        assert!(index.get(alias, 7, 3).is_none());
    }

    #[test]
    fn finds_alias_for_canonical() {
        let mut index = EdgeAliasIndex::init(VectorMemory::default());
        let alias = VertexId::from(2);
        let canonical = VertexId::from(1);

        index.insert(alias, 7, 3, canonical, 9);

        assert_eq!(
            index.find_alias_for_canonical(canonical, 7, 9),
            Some((alias, 3))
        );
        assert_eq!(index.find_alias_for_canonical(canonical, 8, 9), None);
    }

    #[test]
    fn raw_payload_bytes_uses_fixed_key_and_value_widths() {
        let mut index = EdgeAliasIndex::init(VectorMemory::default());
        assert_eq!(index.raw_payload_bytes(), 0);
        index.insert(VertexId::from(2), 7, 3, VertexId::from(1), 9);
        assert_eq!(index.raw_payload_bytes(), EDGE_ALIAS_RAW_ENTRY_BYTES);
    }
}
