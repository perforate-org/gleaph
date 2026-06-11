use gleaph_graph_kernel::entry::RemoteRefId;
use gleaph_graph_kernel::federation::GlobalVertexId;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::cell::Cell;

pub struct RemoteVertexRefTable<M: Memory> {
    ref_to_vertex: StableBTreeMap<RemoteRefId, GlobalVertexId, M>,
    vertex_to_ref: StableBTreeMap<GlobalVertexId, RemoteRefId, M>,
    next_ref_id: Cell<u32>,
}

impl<M: Memory> RemoteVertexRefTable<M> {
    pub fn init(ref_to_vertex_memory: M, vertex_to_ref_memory: M) -> Self {
        Self {
            ref_to_vertex: StableBTreeMap::init(ref_to_vertex_memory),
            vertex_to_ref: StableBTreeMap::init(vertex_to_ref_memory),
            next_ref_id: Cell::new(1),
        }
    }

    pub fn ensure_remote_ref(&mut self, vertex_id: GlobalVertexId) -> RemoteRefId {
        if let Some(existing) = self.vertex_to_ref.get(&vertex_id) {
            return existing;
        }
        let ref_id = RemoteRefId::from_raw(self.allocate_ref_id());
        self.ref_to_vertex.insert(ref_id, vertex_id);
        self.vertex_to_ref.insert(vertex_id, ref_id);
        ref_id
    }

    pub fn global_vertex_id(&self, remote_ref: RemoteRefId) -> Option<GlobalVertexId> {
        self.ref_to_vertex.get(&remote_ref)
    }

    pub fn remote_ref_for_vertex(&self, vertex_id: GlobalVertexId) -> Option<RemoteRefId> {
        self.vertex_to_ref.get(&vertex_id)
    }

    fn allocate_ref_id(&self) -> u32 {
        let id = self.next_ref_id.get();
        self.next_ref_id.set(id.saturating_add(1));
        id
    }
}
