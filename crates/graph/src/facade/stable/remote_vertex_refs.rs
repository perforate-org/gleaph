use gleaph_graph_kernel::entry::RemoteRefId;
use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RemoteRefKey(u32);

impl RemoteRefKey {
    fn new(id: RemoteRefId) -> Self {
        Self(id.raw())
    }
}

impl Storable for RemoteRefKey {
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

pub struct RemoteVertexRefTable<M: Memory> {
    ref_to_logical: StableBTreeMap<RemoteRefKey, StoredLogicalVertexId, M>,
    logical_to_ref: StableBTreeMap<StoredLogicalVertexId, RemoteRefKey, M>,
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
        let key = StoredLogicalVertexId(logical_vertex_id);
        if let Some(existing) = self.logical_to_ref.get(&key) {
            return RemoteRefId::from_raw(existing.0);
        }
        let ref_id = RemoteRefId::from_raw(self.allocate_ref_id());
        self.ref_to_logical
            .insert(RemoteRefKey::new(ref_id), key);
        self.logical_to_ref.insert(key, RemoteRefKey::new(ref_id));
        ref_id
    }

    pub fn logical_vertex_id(&self, remote_ref: RemoteRefId) -> Option<LogicalVertexId> {
        self.ref_to_logical
            .get(&RemoteRefKey::new(remote_ref))
            .map(|stored| stored.0)
    }

    fn allocate_ref_id(&self) -> u32 {
        let id = self.next_ref_id.get();
        self.next_ref_id.set(id.saturating_add(1));
        id
    }
}
