use gleaph_graph_kernel::federation::{LocalVertexId, LogicalVertexId};
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap};

fn local_vertex_id(vertex_id: VertexId) -> LocalVertexId {
    u32::from_le_bytes(vertex_id.to_le_bytes())
}

pub struct VertexLogicalIdMap<M: Memory> {
    map: StableBTreeMap<LocalVertexId, LogicalVertexId, M>,
}

impl<M: Memory> VertexLogicalIdMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, vertex_id: VertexId) -> Option<LogicalVertexId> {
        self.map.get(&local_vertex_id(vertex_id))
    }

    pub fn insert(&mut self, vertex_id: VertexId, logical_vertex_id: LogicalVertexId) {
        self.map
            .insert(local_vertex_id(vertex_id), logical_vertex_id);
    }

    pub fn remove(&mut self, vertex_id: VertexId) {
        self.map.remove(&local_vertex_id(vertex_id));
    }

    pub fn find_vertex_id(&self, logical_vertex_id: LogicalVertexId) -> Option<VertexId> {
        self.map.iter().find_map(|entry| {
            (entry.value() == logical_vertex_id).then_some(VertexId::from(*entry.key()))
        })
    }
}
