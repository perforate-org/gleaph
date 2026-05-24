//! Stable storage for incremental vertex migration.

use gleaph_graph_kernel::federation::{
    LocalVertexId, LogicalVertexId, MigrationEdgeHandleWire, MigrationItem, MigrationJournalEntry,
    PruneMigratedSourceItem, VertexMigrationState,
};
use ic_stable_structures::{Memory, StableBTreeMap, Storable};
use std::borrow::Cow;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MigrationJournalKey {
    logical_vertex_id: LogicalVertexId,
    epoch: u64,
    seq: u64,
}

impl Storable for MigrationJournalKey {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Bounded {
            max_size: 24,
            is_fixed_size: true,
        };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 24];
        out[0..8].copy_from_slice(&self.logical_vertex_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.epoch.to_le_bytes());
        out[16..24].copy_from_slice(&self.seq.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        let mut logical = [0u8; 8];
        let mut epoch = [0u8; 8];
        let mut seq = [0u8; 8];
        logical.copy_from_slice(&b[0..8]);
        epoch.copy_from_slice(&b[8..16]);
        seq.copy_from_slice(&b[16..24]);
        Self {
            logical_vertex_id: u64::from_le_bytes(logical),
            epoch: u64::from_le_bytes(epoch),
            seq: u64::from_le_bytes(seq),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct MigrationOutHandleKey {
    logical_vertex_id: LogicalVertexId,
    epoch: u64,
    owner_local_vertex_id: LocalVertexId,
    label_raw: u32,
    slot_index: u32,
}

impl MigrationOutHandleKey {
    fn from_wire(logical: LogicalVertexId, epoch: u64, handle: MigrationEdgeHandleWire) -> Self {
        Self {
            logical_vertex_id: logical,
            epoch,
            owner_local_vertex_id: handle.owner_local_vertex_id,
            label_raw: handle.label_raw,
            slot_index: handle.slot_index,
        }
    }
}

impl Storable for MigrationOutHandleKey {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Bounded {
            max_size: 28,
            is_fixed_size: true,
        };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 28];
        out[0..8].copy_from_slice(&self.logical_vertex_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.epoch.to_le_bytes());
        out[16..20].copy_from_slice(&self.owner_local_vertex_id.to_le_bytes());
        out[20..24].copy_from_slice(&self.label_raw.to_le_bytes());
        out[24..28].copy_from_slice(&self.slot_index.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        let mut logical = [0u8; 8];
        let mut epoch = [0u8; 8];
        let mut owner = [0u8; 4];
        let mut label = [0u8; 4];
        let mut slot = [0u8; 4];
        logical.copy_from_slice(&b[0..8]);
        epoch.copy_from_slice(&b[8..16]);
        owner.copy_from_slice(&b[16..20]);
        label.copy_from_slice(&b[20..24]);
        slot.copy_from_slice(&b[24..28]);
        Self {
            logical_vertex_id: u64::from_le_bytes(logical),
            epoch: u64::from_le_bytes(epoch),
            owner_local_vertex_id: u32::from_le_bytes(owner),
            label_raw: u32::from_le_bytes(label),
            slot_index: u32::from_le_bytes(slot),
        }
    }
}

pub struct VertexMigrationStateMap<M: Memory> {
    map: StableBTreeMap<LocalVertexId, VertexMigrationState, M>,
}

impl<M: Memory> VertexMigrationStateMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, local: LocalVertexId) -> Option<VertexMigrationState> {
        self.map.get(&local)
    }

    pub fn insert(&mut self, local: LocalVertexId, state: VertexMigrationState) {
        self.map.insert(local, state);
    }

    pub fn remove(&mut self, local: LocalVertexId) {
        self.map.remove(&local);
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn for_each<F>(&self, mut visit: F)
    where
        F: FnMut(LocalVertexId, VertexMigrationState),
    {
        for entry in self.map.iter() {
            visit(*entry.key(), entry.value());
        }
    }
}

pub struct MigrationQueueMap<M: Memory> {
    map: StableBTreeMap<LogicalVertexId, MigrationItem, M>,
}

impl<M: Memory> MigrationQueueMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, logical: LogicalVertexId) -> Option<MigrationItem> {
        self.map.get(&logical)
    }

    pub fn insert(&mut self, logical: LogicalVertexId, item: MigrationItem) {
        self.map.insert(logical, item);
    }

    pub fn remove(&mut self, logical: LogicalVertexId) {
        self.map.remove(&logical);
    }

    pub fn first_item(&self) -> Option<(LogicalVertexId, MigrationItem)> {
        self.map
            .iter()
            .next()
            .map(|entry| (*entry.key(), entry.value().clone()))
    }

    #[cfg(test)]
    pub fn logical_ids(&self) -> Vec<LogicalVertexId> {
        self.map.iter().map(|entry| *entry.key()).collect()
    }
}

pub struct PruneMigratedSourceQueueMap<M: Memory> {
    map: StableBTreeMap<LogicalVertexId, PruneMigratedSourceItem, M>,
}

impl<M: Memory> PruneMigratedSourceQueueMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, logical: LogicalVertexId) -> Option<PruneMigratedSourceItem> {
        self.map.get(&logical)
    }

    pub fn insert(&mut self, logical: LogicalVertexId, item: PruneMigratedSourceItem) {
        self.map.insert(logical, item);
    }

    pub fn remove(&mut self, logical: LogicalVertexId) {
        self.map.remove(&logical);
    }

    pub fn first_item(&self) -> Option<(LogicalVertexId, PruneMigratedSourceItem)> {
        self.map
            .iter()
            .next()
            .map(|entry| (*entry.key(), entry.value().clone()))
    }

    #[cfg(test)]
    pub fn logical_ids(&self) -> Vec<LogicalVertexId> {
        self.map.iter().map(|entry| *entry.key()).collect()
    }
}

pub struct MigrationJournalMap<M: Memory> {
    map: StableBTreeMap<MigrationJournalKey, MigrationJournalEntry, M>,
}

impl<M: Memory> MigrationJournalMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn append(&mut self, entry: MigrationJournalEntry) {
        let key = MigrationJournalKey {
            logical_vertex_id: entry.logical_vertex_id,
            epoch: entry.epoch,
            seq: entry.seq,
        };
        self.map.insert(key, entry);
    }

    pub fn entries_for(
        &self,
        logical: LogicalVertexId,
        epoch: u64,
        from_seq: u64,
        through_seq: u64,
    ) -> Vec<MigrationJournalEntry> {
        let mut out = Vec::new();
        for seq in from_seq..=through_seq {
            let key = MigrationJournalKey {
                logical_vertex_id: logical,
                epoch,
                seq,
            };
            if let Some(entry) = self.map.get(&key) {
                out.push(entry);
            }
        }
        out
    }

    pub fn remove_migration(&mut self, logical: LogicalVertexId, epoch: u64) {
        let keys: Vec<MigrationJournalKey> = self
            .map
            .iter()
            .filter_map(|entry| {
                let k = entry.key();
                (k.logical_vertex_id == logical && k.epoch == epoch).then_some(*k)
            })
            .collect();
        for k in keys {
            self.map.remove(&k);
        }
    }

    pub fn count_for(&self, logical: LogicalVertexId, epoch: u64) -> u64 {
        let mut n = 0u64;
        for entry in self.map.iter() {
            if entry.key().logical_vertex_id == logical && entry.value().epoch == epoch {
                n = n.max(entry.value().seq + 1);
            }
        }
        n
    }
}

pub struct MigrationOutHandleMap<M: Memory> {
    map: StableBTreeMap<MigrationOutHandleKey, MigrationEdgeHandleWire, M>,
}

impl<M: Memory> MigrationOutHandleMap<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(
        &mut self,
        logical: LogicalVertexId,
        epoch: u64,
        source: MigrationEdgeHandleWire,
        target: MigrationEdgeHandleWire,
    ) {
        let key = MigrationOutHandleKey::from_wire(logical, epoch, source);
        self.map.insert(key, target);
    }

    pub fn get(
        &self,
        logical: LogicalVertexId,
        epoch: u64,
        source: MigrationEdgeHandleWire,
    ) -> Option<MigrationEdgeHandleWire> {
        self.map
            .get(&MigrationOutHandleKey::from_wire(logical, epoch, source))
    }

    pub fn remove_migration(&mut self, logical: LogicalVertexId, epoch: u64) {
        let keys: Vec<MigrationOutHandleKey> = self
            .map
            .iter()
            .filter_map(|entry| {
                let k = entry.key();
                (k.logical_vertex_id == logical && k.epoch == epoch).then_some(*k)
            })
            .collect();
        for k in keys {
            self.map.remove(&k);
        }
    }

    pub fn has_migration(&self, logical: LogicalVertexId, epoch: u64) -> bool {
        self.map.iter().any(|entry| {
            let k = entry.key();
            k.logical_vertex_id == logical && k.epoch == epoch
        })
    }
}

pub type MigrationRevHandleMap<M> = MigrationOutHandleMap<M>;
