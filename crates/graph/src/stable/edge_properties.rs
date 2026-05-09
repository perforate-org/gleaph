use super::vertex_properties::{StoredPropertyValue, VertexPropertyStoreError};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, VertexEdgeId};
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::{borrow::Cow, ops::Bound as RangeBound};

fn edge_property_key_range(
    owner_vertex_id: VertexId,
    vertex_edge_id: VertexEdgeId,
) -> (RangeBound<EdgePropertyKey>, RangeBound<EdgePropertyKey>) {
    let owner_vertex_id = u32::from_le_bytes(owner_vertex_id.to_le_bytes());
    let start = EdgePropertyKey {
        owner_vertex_id,
        vertex_edge_id,
        property_id: PropertyId::from_raw(0),
    };
    let upper = vertex_edge_id.raw().checked_add(1).map(|next_edge_id| {
        RangeBound::Excluded(EdgePropertyKey {
            owner_vertex_id,
            vertex_edge_id: VertexEdgeId::from_raw(next_edge_id),
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
    vertex_edge_id: VertexEdgeId,
    property_id: PropertyId,
}

impl EdgePropertyKey {
    pub fn new(
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Self {
        Self {
            owner_vertex_id: u32::from_le_bytes(owner_vertex_id.to_le_bytes()),
            vertex_edge_id,
            property_id,
        }
    }

    pub fn owner_vertex_id(self) -> VertexId {
        VertexId::from(self.owner_vertex_id)
    }

    pub fn vertex_edge_id(self) -> VertexEdgeId {
        self.vertex_edge_id
    }

    pub fn property_id(self) -> PropertyId {
        self.property_id
    }
}

impl Storable for EdgePropertyKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 12,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&self.owner_vertex_id.to_be_bytes());
        out.extend_from_slice(&self.vertex_edge_id.raw().to_be_bytes());
        out.extend_from_slice(&self.property_id.raw().to_be_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 12, "EdgePropertyKey expects exactly 12 bytes");

        let mut owner = [0; 4];
        let mut edge = [0; 4];
        let mut property = [0; 4];
        owner.copy_from_slice(&bytes[0..4]);
        edge.copy_from_slice(&bytes[4..8]);
        property.copy_from_slice(&bytes[8..12]);

        Self {
            owner_vertex_id: u32::from_be_bytes(owner),
            vertex_edge_id: VertexEdgeId::from_raw(u32::from_be_bytes(edge)),
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
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        if property_id.raw() == 0 {
            return None;
        }
        self.properties
            .get(&EdgePropertyKey::new(
                owner_vertex_id,
                vertex_edge_id,
                property_id,
            ))
            .map(|value| value.0)
    }

    pub fn set(
        &mut self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
        value: Value,
    ) -> Result<Option<Value>, VertexPropertyStoreError> {
        if property_id.raw() == 0 {
            return Err(VertexPropertyStoreError::ReservedPropertyId(property_id));
        }
        value.to_binary_bytes()?;
        Ok(self
            .properties
            .insert(
                EdgePropertyKey::new(owner_vertex_id, vertex_edge_id, property_id),
                StoredPropertyValue(value),
            )
            .map(|previous| previous.0))
    }

    pub fn remove(
        &mut self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
        property_id: PropertyId,
    ) -> Option<Value> {
        if property_id.raw() == 0 {
            return None;
        }
        self.properties
            .remove(&EdgePropertyKey::new(
                owner_vertex_id,
                vertex_edge_id,
                property_id,
            ))
            .map(|value| value.0)
    }

    pub fn properties_for_edge(
        &self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> Vec<(PropertyId, Value)> {
        let range = edge_property_key_range(owner_vertex_id, vertex_edge_id);
        let owner = owner_vertex_id;
        self.properties
            .range(range)
            .take_while(|entry| {
                let key = entry.key();
                key.owner_vertex_id() == owner && key.vertex_edge_id() == vertex_edge_id
            })
            .map(|entry| {
                let (key, value) = entry.into_pair();
                (key.property_id(), value.0)
            })
            .collect()
    }

    /// Removes every property entry for one logical edge and returns the removed count.
    pub fn remove_all_for_edge(
        &mut self,
        owner_vertex_id: VertexId,
        vertex_edge_id: VertexEdgeId,
    ) -> u32 {
        let range = edge_property_key_range(owner_vertex_id, vertex_edge_id);
        let owner = owner_vertex_id;
        let mut removed = 0u32;

        loop {
            let Some(entry) = self.properties.range(range.clone()).next() else {
                return removed;
            };
            let key = *entry.key();
            if key.owner_vertex_id() != owner || key.vertex_edge_id() != vertex_edge_id {
                return removed;
            }
            self.properties.remove(&key);
            removed = removed.saturating_add(1);
        }
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
        let edge = VertexEdgeId::from_raw(3);
        let weight = PropertyId::from_raw(1);

        assert_eq!(store.get(owner, edge, weight), None);
        assert_eq!(
            store.set(owner, edge, weight, Value::Int64(10)).unwrap(),
            None
        );
        assert_eq!(store.get(owner, edge, weight), Some(Value::Int64(10)));
        assert_eq!(
            store.set(owner, edge, weight, Value::Int64(20)).unwrap(),
            Some(Value::Int64(10))
        );
        assert_eq!(store.get(owner, edge, weight), Some(Value::Int64(20)));
    }

    #[test]
    fn remove_edge_property() {
        let mut store = store();
        let owner = VertexId::from(7);
        let edge = VertexEdgeId::from_raw(3);
        let weight = PropertyId::from_raw(1);

        store.set(owner, edge, weight, Value::Int64(10)).unwrap();

        assert_eq!(store.remove(owner, edge, weight), Some(Value::Int64(10)));
        assert_eq!(store.remove(owner, edge, weight), None);
        assert_eq!(store.get(owner, edge, weight), None);
    }

    #[test]
    fn properties_for_edge_returns_only_one_owner_edge_pair() {
        let mut store = store();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);
        let first = VertexEdgeId::from_raw(3);
        let second = VertexEdgeId::from_raw(4);
        let weight = PropertyId::from_raw(1);
        let since = PropertyId::from_raw(2);

        store
            .set(alice, first, since, Value::Int64(2026))
            .expect("set first since");
        store
            .set(alice, first, weight, Value::Int64(10))
            .expect("set first weight");
        store
            .set(alice, second, weight, Value::Int64(20))
            .expect("set second weight");
        store
            .set(bob, first, weight, Value::Int64(30))
            .expect("set bob first weight");

        assert_eq!(
            store.properties_for_edge(alice, first),
            vec![(weight, Value::Int64(10)), (since, Value::Int64(2026)),]
        );
    }

    #[test]
    fn properties_for_edge_handles_max_vertex_edge_id() {
        let mut store = store();
        let owner = VertexId::from(u32::MAX);
        let edge = VertexEdgeId::from_raw(u32::MAX);
        let weight = PropertyId::from_raw(1);

        store
            .set(owner, edge, weight, Value::Int64(10))
            .expect("set max edge property");

        assert_eq!(
            store.properties_for_edge(owner, edge),
            vec![(weight, Value::Int64(10))]
        );
    }

    #[test]
    fn remove_all_for_edge_removes_only_one_owner_edge_pair() {
        let mut store = store();
        let alice = VertexId::from(7);
        let bob = VertexId::from(8);
        let first = VertexEdgeId::from_raw(3);
        let second = VertexEdgeId::from_raw(4);
        let weight = PropertyId::from_raw(1);
        let since = PropertyId::from_raw(2);

        store.set(alice, first, weight, Value::Int64(10)).unwrap();
        store.set(alice, first, since, Value::Int64(2026)).unwrap();
        store.set(alice, second, weight, Value::Int64(11)).unwrap();
        store.set(bob, first, weight, Value::Int64(12)).unwrap();

        assert_eq!(store.remove_all_for_edge(alice, first), 2);
        assert!(store.properties_for_edge(alice, first).is_empty());
        assert_eq!(
            store.properties_for_edge(alice, second),
            vec![(weight, Value::Int64(11))]
        );
        assert_eq!(
            store.properties_for_edge(bob, first),
            vec![(weight, Value::Int64(12))]
        );
    }

    #[test]
    fn persists_across_reopen() {
        let mut store = store();
        let owner = VertexId::from(7);
        let edge = VertexEdgeId::from_raw(3);
        let weight = PropertyId::from_raw(1);

        store.set(owner, edge, weight, Value::Int64(10)).unwrap();
        let memory = store.into_memory();

        let reopened = EdgePropertyStore::init(memory);

        assert_eq!(reopened.get(owner, edge, weight), Some(Value::Int64(10)));
    }

    #[test]
    fn rejects_reserved_property_id() {
        let mut store = store();

        assert!(matches!(
            store.set(
                VertexId::from(7),
                VertexEdgeId::from_raw(3),
                PropertyId::default(),
                Value::Null
            ),
            Err(VertexPropertyStoreError::ReservedPropertyId(id)) if id.raw() == 0
        ));
        assert_eq!(
            store.get(
                VertexId::from(7),
                VertexEdgeId::from_raw(3),
                PropertyId::default()
            ),
            None
        );
    }
}
