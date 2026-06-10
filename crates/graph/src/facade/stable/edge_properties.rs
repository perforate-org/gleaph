use super::vertex_properties::{StoredPropertyValue, VertexPropertyStoreError};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, ops::Bound as RangeBound};

fn edge_property_key_range(
    owner_vertex_id: VertexId,
    label_id: u16,
    slot_index: u32,
) -> (RangeBound<EdgePropertyKey>, RangeBound<EdgePropertyKey>) {
    let owner_vertex_id = u32::from_le_bytes(owner_vertex_id.to_le_bytes());
    let start = EdgePropertyKey {
        owner_vertex_id,
        label_id,
        slot_index,
        property_id: PropertyId::from_raw(0),
    };
    let upper = slot_index.checked_add(1).map(|next_slot| {
        RangeBound::Excluded(EdgePropertyKey {
            owner_vertex_id,
            label_id,
            slot_index: next_slot,
            property_id: PropertyId::from_raw(0),
        })
    });
    match upper {
        Some(upper) => (RangeBound::Included(start), upper),
        None => (RangeBound::Included(start), RangeBound::Unbounded),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgePropertyKey {
    owner_vertex_id: u32,
    label_id: u16,
    slot_index: u32,
    property_id: PropertyId,
}

impl EdgePropertyKey {
    pub fn new(
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
        property_id: PropertyId,
    ) -> Self {
        Self {
            owner_vertex_id: u32::from_le_bytes(owner_vertex_id.to_le_bytes()),
            label_id,
            slot_index,
            property_id,
        }
    }

    pub fn owner_vertex_id(self) -> VertexId {
        VertexId::from(self.owner_vertex_id)
    }

    pub fn label_id(self) -> u16 {
        self.label_id
    }

    pub fn slot_index(self) -> u32 {
        self.slot_index
    }

    pub fn property_id(self) -> PropertyId {
        self.property_id
    }
}

impl Storable for EdgePropertyKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 14,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(14);
        out.extend_from_slice(&self.owner_vertex_id.to_be_bytes());
        out.extend_from_slice(&self.label_id.to_be_bytes());
        out.extend_from_slice(&self.slot_index.to_be_bytes());
        out.extend_from_slice(&self.property_id.raw().to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 14, "EdgePropertyKey expects exactly 14 bytes");

        let mut owner = [0; 4];
        let mut label = [0; 2];
        let mut slot = [0; 4];
        let mut property = [0; 4];
        owner.copy_from_slice(&bytes[0..4]);
        label.copy_from_slice(&bytes[4..6]);
        slot.copy_from_slice(&bytes[6..10]);
        property.copy_from_slice(&bytes[10..14]);

        Self {
            owner_vertex_id: u32::from_be_bytes(owner),
            label_id: u16::from_be_bytes(label),
            slot_index: u32::from_be_bytes(slot),
            property_id: PropertyId::from_raw(u32::from_be_bytes(property)),
        }
    }
}

pub struct EdgePropertyStore<M: Memory> {
    properties: StableBTreeMap<EdgePropertyKey, StoredPropertyValue, M>,
}

impl<M: Memory> EdgePropertyStore<M> {
    pub fn init(memory: M) -> Self {
        Self {
            properties: StableBTreeMap::init(memory),
        }
    }

    pub fn get(
        &self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
        property_id: PropertyId,
    ) -> Option<Value> {
        if property_id.raw() == 0 {
            return None;
        }
        self.properties
            .get(&EdgePropertyKey::new(
                owner_vertex_id,
                label_id,
                slot_index,
                property_id,
            ))
            .map(|value| value.0)
    }

    pub fn set(
        &mut self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        crate::property::ensure_property_id(property_id)
            .map_err(VertexPropertyStoreError::ReservedPropertyId)?;
        crate::property::ensure_persistable(&value)
            .map_err(VertexPropertyStoreError::InvalidValue)?;
        Ok(self
            .properties
            .insert(
                EdgePropertyKey::new(owner_vertex_id, label_id, slot_index, property_id),
                StoredPropertyValue(value),
            )
            .map(|previous| previous.0))
    }

    pub fn remove(
        &mut self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
        property_id: PropertyId,
    ) -> Option<Value> {
        if property_id.raw() == 0 {
            return None;
        }
        self.properties
            .remove(&EdgePropertyKey::new(
                owner_vertex_id,
                label_id,
                slot_index,
                property_id,
            ))
            .map(|value| value.0)
    }

    pub fn properties_for_edge(
        &self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) -> Vec<(PropertyId, Value)> {
        let mut out = Vec::new();
        self.for_each_property_for_edge(owner_vertex_id, label_id, slot_index, |pid, v| {
            out.push((pid, v));
        });
        out
    }

    pub(crate) fn for_each_property_for_edge<F>(
        &self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
        mut f: F,
    ) where
        F: FnMut(PropertyId, Value),
    {
        let range = edge_property_key_range(owner_vertex_id, label_id, slot_index);
        let owner = owner_vertex_id;
        for entry in self.properties.range(range).take_while(|entry| {
            let key = entry.key();
            key.owner_vertex_id() == owner
                && key.label_id() == label_id
                && key.slot_index() == slot_index
        }) {
            let (key, value) = entry.into_pair();
            f(key.property_id(), value.0);
        }
    }

    /// Removes every property entry for one logical edge and returns the removed count.
    pub fn remove_all_for_edge(
        &mut self,
        owner_vertex_id: VertexId,
        label_id: u16,
        slot_index: u32,
    ) -> u32 {
        let range = edge_property_key_range(owner_vertex_id, label_id, slot_index);
        let owner = owner_vertex_id;
        let mut removed = 0u32;

        loop {
            let Some(entry) = self.properties.range(range).next() else {
                return removed;
            };
            let key = *entry.key();
            if key.owner_vertex_id() != owner
                || key.label_id() != label_id
                || key.slot_index() != slot_index
            {
                return removed;
            }
            self.properties.remove(&key);
            removed = removed.saturating_add(1);
        }
    }

    pub fn move_all_for_edge(
        &mut self,
        owner_vertex_id: VertexId,
        label_id: u16,
        old_slot_index: u32,
        new_slot_index: u32,
    ) -> Result<Vec<(PropertyId, Value)>, VertexPropertyStoreError> {
        if old_slot_index == new_slot_index {
            return Ok(Vec::new());
        }
        let properties = self.properties_for_edge(owner_vertex_id, label_id, old_slot_index);
        if properties.is_empty() {
            return Ok(Vec::new());
        }
        self.remove_all_for_edge(owner_vertex_id, label_id, old_slot_index);
        for (property_id, value) in &properties {
            self.set(
                owner_vertex_id,
                label_id,
                new_slot_index,
                *property_id,
                value.clone(),
            )?;
        }
        Ok(properties)
    }

    pub fn into_memory(self) -> M {
        self.properties.into_memory()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    fn store() -> EdgePropertyStore<VectorMemory> {
        EdgePropertyStore::init(VectorMemory::default())
    }

    #[test]
    fn set_get_and_replace_edge_property() {
        let mut store = store();
        let owner = VertexId::from(7);
        let edge = 3;
        let weight = PropertyId::from_raw(1);

        assert_eq!(store.get(owner, 0, edge, weight), None);
        assert_eq!(
            store.set(owner, 0, edge, weight, Value::Int64(10)).unwrap(),
            None
        );
        assert_eq!(store.get(owner, 0, edge, weight), Some(Value::Int64(10)));
        assert_eq!(
            store.set(owner, 0, edge, weight, Value::Int64(20)).unwrap(),
            Some(Value::Int64(10))
        );
        assert_eq!(store.get(owner, 0, edge, weight), Some(Value::Int64(20)));
    }

    #[test]
    fn remove_edge_property() {
        let mut store = store();
        let owner = VertexId::from(7);
        let edge = 3;
        let weight = PropertyId::from_raw(1);

        store.set(owner, 0, edge, weight, Value::Int64(10)).unwrap();

        assert_eq!(store.remove(owner, 0, edge, weight), Some(Value::Int64(10)));
        assert_eq!(store.remove(owner, 0, edge, weight), None);
        assert_eq!(store.get(owner, 0, edge, weight), None);
    }

    #[test]
    fn properties_for_edge_returns_only_one_owner_edge_pair() {
        let mut store = store();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);
        let first = 3;
        let second = 4;
        let weight = PropertyId::from_raw(1);
        let since = PropertyId::from_raw(2);

        store
            .set(alice, 0, first, since, Value::Int64(2026))
            .expect("set first since");
        store
            .set(alice, 0, first, weight, Value::Int64(10))
            .expect("set first weight");
        store
            .set(alice, 0, second, weight, Value::Int64(20))
            .expect("set second weight");
        store
            .set(bob, 0, first, weight, Value::Int64(30))
            .expect("set bob first weight");

        assert_eq!(
            store.properties_for_edge(alice, 0, first),
            vec![(weight, Value::Int64(10)), (since, Value::Int64(2026)),]
        );
    }

    #[test]
    fn properties_for_edge_handles_max_edge_slot_index() {
        let mut store = store();
        let owner = VertexId::from(u32::MAX);
        let edge = u32::MAX;
        let weight = PropertyId::from_raw(1);

        store
            .set(owner, 0, edge, weight, Value::Int64(10))
            .expect("set max edge property");

        assert_eq!(
            store.properties_for_edge(owner, 0, edge),
            vec![(weight, Value::Int64(10))]
        );
    }

    #[test]
    fn remove_all_for_edge_removes_only_one_owner_edge_pair() {
        let mut store = store();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);
        let first = 3;
        let second = 4;
        let weight = PropertyId::from_raw(1);
        let since = PropertyId::from_raw(2);

        store
            .set(alice, 0, first, weight, Value::Int64(10))
            .unwrap();
        store
            .set(alice, 0, first, since, Value::Int64(2026))
            .unwrap();
        store
            .set(alice, 0, second, weight, Value::Int64(11))
            .unwrap();
        store.set(bob, 0, first, weight, Value::Int64(12)).unwrap();

        assert_eq!(store.remove_all_for_edge(alice, 0, first), 2);
        assert!(store.properties_for_edge(alice, 0, first).is_empty());
        assert_eq!(
            store.properties_for_edge(alice, 0, second),
            vec![(weight, Value::Int64(11))]
        );
        assert_eq!(
            store.properties_for_edge(bob, 0, first),
            vec![(weight, Value::Int64(12))]
        );
    }

    #[test]
    fn persists_across_reopen() {
        let mut store = store();
        let owner = VertexId::from(7);
        let edge = 3;
        let weight = PropertyId::from_raw(1);

        store.set(owner, 0, edge, weight, Value::Int64(10)).unwrap();
        let memory = store.into_memory();

        let reopened = EdgePropertyStore::init(memory);

        assert_eq!(reopened.get(owner, 0, edge, weight), Some(Value::Int64(10)));
    }

    #[test]
    fn rejects_reserved_property_id() {
        let mut store = store();

        assert!(matches!(
            store.set(
                VertexId::from(7),
                0,
                3,
                PropertyId::default(),
                Value::Null
            ),
            Err(VertexPropertyStoreError::ReservedPropertyId(id)) if id.raw() == 0
        ));
        assert_eq!(
            store.get(VertexId::from(7), 0, 3, PropertyId::default()),
            None
        );
    }
}
