use gleaph_graph_kernel::federation::{GlobalVertexId, PhysicalPlacementKey};
use ic_stable_structures::{Memory, StableBTreeMap};

pub struct PlacementByPhysicalMap<M: Memory> {
    map: StableBTreeMap<PhysicalPlacementKey, GlobalVertexId, M>,
}

impl<M: Memory> PlacementByPhysicalMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(&mut self, key: PhysicalPlacementKey, vertex_id: GlobalVertexId) {
        self.map.insert(key, vertex_id);
    }

    pub fn get(&self, key: PhysicalPlacementKey) -> Option<GlobalVertexId> {
        self.map.get(&key)
    }

    pub fn remove(&mut self, key: PhysicalPlacementKey) {
        self.map.remove(&key);
    }

    pub fn clear_new(&mut self) {
        self.map.clear_new();
    }
}
