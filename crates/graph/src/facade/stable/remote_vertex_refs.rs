use gleaph_graph_kernel::entry::RemoteRefId;
use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_structures::{Memory, StableBTreeMap};
use std::cell::Cell;

pub struct RemoteVertexRefTable<M: Memory> {
    ref_to_logical: StableBTreeMap<RemoteRefId, LogicalVertexId, M>,
    logical_to_ref: StableBTreeMap<LogicalVertexId, RemoteRefId, M>,
    next_ref_id: Cell<u32>,
}

impl<M: Memory> RemoteVertexRefTable<M> {
    pub fn init(ref_to_logical_memory: M, logical_to_ref_memory: M) -> Self {
        Self {
            ref_to_logical: StableBTreeMap::init(ref_to_logical_memory),
            logical_to_ref: StableBTreeMap::init(logical_to_ref_memory),
            next_ref_id: Cell::new(1),
        }
    }

    pub fn ensure_remote_ref(&mut self, logical_vertex_id: LogicalVertexId) -> RemoteRefId {
        if let Some(existing) = self.logical_to_ref.get(&logical_vertex_id) {
            return existing;
        }
        let ref_id = RemoteRefId::from_raw(self.allocate_ref_id());
        self.ref_to_logical
            .insert(ref_id, logical_vertex_id);
        self.logical_to_ref.insert(logical_vertex_id, ref_id);
        ref_id
    }

    pub fn logical_vertex_id(&self, remote_ref: RemoteRefId) -> Option<LogicalVertexId> {
        self.ref_to_logical.get(&remote_ref)
    }

    pub fn remote_ref_for_logical(&self, logical_vertex_id: LogicalVertexId) -> Option<RemoteRefId> {
        self.logical_to_ref.get(&logical_vertex_id)
    }

    fn allocate_ref_id(&self) -> u32 {
        let id = self.next_ref_id.get();
        self.next_ref_id.set(id.saturating_add(1));
        id
    }
}
