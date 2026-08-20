//! Exact Graph-owned floor for ordinary derived-index work awaiting delivery.

use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

const KEY_LEN: usize = 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IndexPendingFloorOwner {
    RepairJournal = 1,
    DerivedIndexOutbox = 2,
}

impl IndexPendingFloorOwner {
    fn from_tag(tag: u8) -> Result<Self, &'static str> {
        match tag {
            1 => Ok(Self::RepairJournal),
            2 => Ok(Self::DerivedIndexOutbox),
            _ => Err("unknown index pending floor owner tag"),
        }
    }
}

/// One exact durable source-row identity ordered first by mutation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct IndexPendingFloorKey {
    mutation_id: u64,
    owner: IndexPendingFloorOwner,
    source_sequence: u64,
}

impl IndexPendingFloorKey {
    pub(crate) fn new(
        mutation_id: u64,
        owner: IndexPendingFloorOwner,
        source_sequence: u64,
    ) -> Result<Self, &'static str> {
        if mutation_id == 0 {
            return Err("index pending floor keys require a nonzero mutation id");
        }
        Ok(Self {
            mutation_id,
            owner,
            source_sequence,
        })
    }

    pub(crate) fn mutation_id(self) -> u64 {
        self.mutation_id
    }
}

impl Storable for IndexPendingFloorKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: KEY_LEN as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut bytes = [0u8; KEY_LEN];
        bytes[..8].copy_from_slice(&self.mutation_id.to_be_bytes());
        bytes[8] = self.owner as u8;
        bytes[9..].copy_from_slice(&self.source_sequence.to_be_bytes());
        Cow::Owned(bytes.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes: [u8; KEY_LEN] = bytes
            .as_ref()
            .try_into()
            .expect("decode 17-byte IndexPendingFloorKey");
        let mutation_id = u64::from_be_bytes(bytes[..8].try_into().expect("mutation id bytes"));
        let owner = IndexPendingFloorOwner::from_tag(bytes[8])
            .expect("decode IndexPendingFloorKey owner tag");
        let source_sequence =
            u64::from_be_bytes(bytes[9..].try_into().expect("source sequence bytes"));
        Self::new(mutation_id, owner, source_sequence)
            .expect("decode nonzero IndexPendingFloorKey mutation id")
    }
}

pub(crate) struct IndexPendingFloor<M: Memory> {
    map: StableBTreeMap<IndexPendingFloorKey, [u8; 0], M>,
}

impl<M: Memory> IndexPendingFloor<M> {
    pub(crate) fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub(crate) fn min_mutation_id(&self) -> Option<u64> {
        self.map.first_key_value().map(|(key, _)| key.mutation_id())
    }

    pub(crate) fn contains(&self, key: &IndexPendingFloorKey) -> bool {
        self.map.contains_key(key)
    }

    pub(crate) fn insert(&mut self, key: IndexPendingFloorKey) {
        assert!(
            self.map.insert(key, []).is_none(),
            "duplicate index pending floor source identity"
        );
    }

    pub(crate) fn remove(&mut self, key: &IndexPendingFloorKey) {
        assert!(
            self.map.remove(key).is_some(),
            "missing index pending floor source identity"
        );
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> u64 {
        self.map.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;
    use std::panic::catch_unwind;

    #[test]
    fn fixed_keys_order_by_mutation_owner_and_sequence() {
        let repair = IndexPendingFloorKey::new(7, IndexPendingFloorOwner::RepairJournal, u64::MAX)
            .expect("repair key");
        let outbox = IndexPendingFloorKey::new(7, IndexPendingFloorOwner::DerivedIndexOutbox, 0)
            .expect("outbox key");
        let successor = IndexPendingFloorKey::new(8, IndexPendingFloorOwner::RepairJournal, 0)
            .expect("successor key");

        assert_eq!(repair.to_bytes().len(), KEY_LEN);
        assert_eq!(&repair.to_bytes()[..8], &7u64.to_be_bytes());
        assert_eq!(repair.to_bytes()[8], 1);
        assert_eq!(&repair.to_bytes()[9..], &u64::MAX.to_be_bytes());
        assert!(repair < outbox);
        assert!(outbox < successor);
        assert_eq!(IndexPendingFloorKey::from_bytes(repair.to_bytes()), repair);
        assert!(IndexPendingFloorKey::new(0, IndexPendingFloorOwner::RepairJournal, 0).is_err());

        let mut unknown_owner = repair.to_bytes().into_owned();
        unknown_owner[8] = 3;
        assert!(catch_unwind(|| IndexPendingFloorKey::from_bytes(unknown_owner.into())).is_err());
        assert!(catch_unwind(|| IndexPendingFloorKey::from_bytes(vec![0; 16].into())).is_err());
    }

    #[test]
    fn fresh_and_reopen_preserve_exact_floor() {
        let memory = VectorMemory::default();
        let mut floor = IndexPendingFloor::init(memory.clone());
        assert_eq!(floor.min_mutation_id(), None);
        floor.insert(
            IndexPendingFloorKey::new(11, IndexPendingFloorOwner::DerivedIndexOutbox, 2)
                .expect("outbox key"),
        );
        floor.insert(
            IndexPendingFloorKey::new(7, IndexPendingFloorOwner::RepairJournal, 9)
                .expect("repair key"),
        );
        drop(floor);

        let mut reopened = IndexPendingFloor::init(memory);
        assert_eq!(reopened.min_mutation_id(), Some(7));
        reopened.remove(
            &IndexPendingFloorKey::new(7, IndexPendingFloorOwner::RepairJournal, 9)
                .expect("repair key"),
        );
        assert_eq!(reopened.min_mutation_id(), Some(11));
    }
}
