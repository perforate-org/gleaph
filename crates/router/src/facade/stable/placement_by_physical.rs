use gleaph_graph_kernel::federation::{LogicalVertexId, PhysicalPlacementKey};
use ic_stable_structures::{Memory, StableBTreeMap};

pub struct PlacementByPhysicalMap<M: Memory> {
    map: StableBTreeMap<PhysicalPlacementKey, LogicalVertexId, M>,
}

impl<M: Memory> PlacementByPhysicalMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(&mut self, key: PhysicalPlacementKey, logical_vertex_id: LogicalVertexId) {
        self.map.insert(key, logical_vertex_id);
    }

    pub fn get(&self, key: PhysicalPlacementKey) -> Option<LogicalVertexId> {
        self.map.get(&key)
    }

    pub fn clear_new(&mut self) {
        self.map.clear_new();
    }
}
