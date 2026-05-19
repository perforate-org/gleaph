use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_lara::VertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct LocalVertexKey(u32);

impl LocalVertexKey {
    fn new(vertex_id: VertexId) -> Self {
        Self(u32::from_le_bytes(vertex_id.to_le_bytes()))
    }
}

impl Storable for LocalVertexKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0; 4];
        raw.copy_from_slice(bytes.as_ref());
        Self(u32::from_le_bytes(raw))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct StoredLogicalVertexId(LogicalVertexId);

impl Storable for StoredLogicalVertexId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.0.to_le_bytes().to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_le_bytes().to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0; 8];
        raw.copy_from_slice(bytes.as_ref());
        Self(u64::from_le_bytes(raw))
    }
}

pub struct VertexLogicalIdMap<M: Memory> {
    map: StableBTreeMap<LocalVertexKey, StoredLogicalVertexId, M>,
}

impl<M: Memory> VertexLogicalIdMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, vertex_id: VertexId) -> Option<LogicalVertexId> {
        self.map
            .get(&LocalVertexKey::new(vertex_id))
            .map(|stored| stored.0)
    }

    pub fn insert(&mut self, vertex_id: VertexId, logical_vertex_id: LogicalVertexId) {
        self.map.insert(
            LocalVertexKey::new(vertex_id),
            StoredLogicalVertexId(logical_vertex_id),
        );
    }
}
