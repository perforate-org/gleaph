//! Graph-shard-local unique value table for the `ShardLocalGlobal` fast path (ADR 0030 slice 10).
//!
//! When a unique constraint is created on a graph that has exactly one live shard, the Router
//! freezes its enforcement strategy to `ShardLocalGlobal`: graph-wide uniqueness is enforced
//! entirely inside that one owning shard, with **no** Router reservations. This table is the single
//! source of truth for those constraints' claimed values.
//!
//! Each entry maps `(constraint_id, encoded_value) → owner_element_id`. One graph canister hosts
//! exactly one logical graph/shard, so the per-canister table is already graph-scoped and the key
//! needs no `graph_id`. `encoded_value` is the canonical [`encode_unique_value`] output the Router
//! threads through the dispatch, so the local key matches the value the shard actually stores.
//!
//! [`encode_unique_value`]: gleaph_gql_ic::encode_unique_value

use candid::{Decode, Encode};
use gleaph_graph_kernel::entry::ConstraintNameId;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::ops::Bound as RangeBound;

/// Key `(constraint_id, encoded_value)`. `StableBTreeMap` orders entries by the deserialized key's
/// `Ord`, so the derive gives `constraint_id`-major, then lexicographic `encoded_value` ordering —
/// every entry of one constraint forms a contiguous range regardless of the byte encoding below.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalUniqueKey {
    pub constraint_id: ConstraintNameId,
    pub encoded_value: Vec<u8>,
}

impl LocalUniqueKey {
    /// The minimal key of `constraint_id`'s contiguous range (empty value sorts first).
    fn range_start(constraint_id: ConstraintNameId) -> Self {
        Self {
            constraint_id,
            encoded_value: Vec::new(),
        }
    }
}

impl Storable for LocalUniqueKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(2 + self.encoded_value.len());
        out.extend_from_slice(&self.constraint_id.to_le_bytes());
        out.extend_from_slice(&self.encoded_value);
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut id = [0u8; 2];
        id.copy_from_slice(&bytes[0..2]);
        Self {
            constraint_id: ConstraintNameId::from_le_bytes(id),
            encoded_value: bytes[2..].to_vec(),
        }
    }
}

/// Versioned stable envelope (ADR 0007) for the local unique value record.
#[derive(Clone, Debug, candid::CandidType, serde::Serialize, serde::Deserialize)]
enum LocalUniqueStableRecord {
    V1(LocalUniqueRecord),
}

/// The owner of a locally-enforced unique value: the canonical element id that claimed it.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Serialize, serde::Deserialize)]
pub struct LocalUniqueRecord {
    pub owner_element_id: Vec<u8>,
}

impl Storable for LocalUniqueRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&LocalUniqueStableRecord::V1(self.clone()))
                .expect("encode local unique record"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&LocalUniqueStableRecord::V1(self)).expect("encode local unique record")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), LocalUniqueStableRecord).expect("decode local unique record")
        {
            LocalUniqueStableRecord::V1(v1) => v1,
        }
    }
}

/// Progress of one bounded [`purge`](GraphLocalUniqueTable::purge) page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalPurgeProgress {
    pub removed: u64,
    /// `true` once the constraint's range is empty (no more entries to purge).
    pub done: bool,
}

/// The local unique value table: `(constraint_id, encoded_value) → owner_element_id`.
pub struct GraphLocalUniqueTable<M: Memory> {
    map: StableBTreeMap<LocalUniqueKey, LocalUniqueRecord, M>,
}

impl<M: Memory> GraphLocalUniqueTable<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    /// Whether `(constraint_id, encoded_value)` is currently claimed.
    pub fn contains(&self, constraint_id: ConstraintNameId, encoded_value: &[u8]) -> bool {
        self.map.contains_key(&LocalUniqueKey {
            constraint_id,
            encoded_value: encoded_value.to_vec(),
        })
    }

    /// Claims `(constraint_id, encoded_value)` for `owner_element_id`. Caller guarantees the value is
    /// absent (the acquire path preflights every claim before the canonical write); a re-insert under
    /// deterministic replay simply re-records the same owner.
    pub fn insert(
        &mut self,
        constraint_id: ConstraintNameId,
        encoded_value: Vec<u8>,
        owner_element_id: Vec<u8>,
    ) {
        self.map.insert(
            LocalUniqueKey {
                constraint_id,
                encoded_value,
            },
            LocalUniqueRecord { owner_element_id },
        );
    }

    /// Frees `(constraint_id, encoded_value)` **iff** it is currently owned by `owner_element_id`.
    /// The owner match prevents a stale release from freeing a value another element now owns.
    /// Returns whether an entry was removed.
    pub fn remove_if_owner(
        &mut self,
        constraint_id: ConstraintNameId,
        encoded_value: &[u8],
        owner_element_id: &[u8],
    ) -> bool {
        let key = LocalUniqueKey {
            constraint_id,
            encoded_value: encoded_value.to_vec(),
        };
        match self.map.get(&key) {
            Some(record) if record.owner_element_id == owner_element_id => {
                self.map.remove(&key);
                true
            }
            _ => false,
        }
    }

    /// Whether the constraint has no remaining local entries (the DROP completion gate).
    pub fn is_empty(&self, constraint_id: ConstraintNameId) -> bool {
        let (lower, upper) = self.constraint_bounds(constraint_id);
        self.map.range((lower, upper)).next().is_none()
    }

    /// Deletes up to `budget` of the constraint's entries (DROP purge), re-scanning from the range
    /// start each call so successive pages make progress as removed rows disappear. Returns the
    /// number removed and whether the constraint's range is now empty.
    pub fn purge(&mut self, constraint_id: ConstraintNameId, budget: usize) -> LocalPurgeProgress {
        let (lower, upper) = self.constraint_bounds(constraint_id);
        let keys: Vec<LocalUniqueKey> = self
            .map
            .range((lower, upper))
            .take(budget)
            .map(|entry| entry.key().clone())
            .collect();
        let removed = keys.len() as u64;
        for key in keys {
            self.map.remove(&key);
        }
        LocalPurgeProgress {
            removed,
            done: self.is_empty(constraint_id),
        }
    }

    /// Half-open key bounds covering exactly `constraint_id`'s contiguous range. At
    /// `ConstraintNameId::MAX` there is no next id, so the upper bound is `Unbounded`.
    fn constraint_bounds(
        &self,
        constraint_id: ConstraintNameId,
    ) -> (RangeBound<LocalUniqueKey>, RangeBound<LocalUniqueKey>) {
        let lower = RangeBound::Included(LocalUniqueKey::range_start(constraint_id));
        let upper = match constraint_id.raw().checked_add(1) {
            Some(next) => RangeBound::Excluded(LocalUniqueKey::range_start(
                ConstraintNameId::from_raw(next),
            )),
            None => RangeBound::Unbounded,
        };
        (lower, upper)
    }
}
