//! Stable label stats delta log and graph mutation journal (ADR 0015).

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::LocalVertexId;
use gleaph_graph_kernel::plan_exec::{
    GraphMutationJournalEntryWire, LabelStatsDeltaEventWire, MutationId, MutationJournalState,
    ShardEventSeq,
};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;
use std::ops::Bound as StdBound;

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct GraphMutationJournalEntry {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub row_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    pub hot_forward_vertices: Vec<LocalVertexId>,
    /// IC time (ns) when this entry was last recorded, the sole basis for time-based
    /// retention (ADR 0027). `None` decodes from pre-ADR-0027 bytes (Candid omits the
    /// field on legacy values); the amortized sweep lazy-stamps such entries to "now" so
    /// the pre-upgrade backlog ages out from upgrade time rather than evicting prematurely.
    #[serde(default)]
    pub recorded_at_ns: Option<u64>,
}

impl GraphMutationJournalEntry {
    pub fn incomplete(
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        recorded_at_ns: u64,
    ) -> Self {
        Self {
            mutation_id,
            state: MutationJournalState::Incomplete,
            row_count: 0,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: Vec::new(),
            recorded_at_ns: Some(recorded_at_ns),
        }
    }

    pub fn completed(
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
        recorded_at_ns: u64,
    ) -> Self {
        Self {
            mutation_id,
            state: MutationJournalState::Completed,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            recorded_at_ns: Some(recorded_at_ns),
        }
    }

    pub fn wire(&self) -> GraphMutationJournalEntryWire {
        GraphMutationJournalEntryWire {
            mutation_id: self.mutation_id,
            state: self.state,
            row_count: self.row_count,
            emitted_delta_first_seq: self.emitted_delta_first_seq,
            emitted_delta_last_seq: self.emitted_delta_last_seq,
            hot_forward_vertices: self.hot_forward_vertices.clone(),
        }
    }

    pub fn is_completed(&self) -> bool {
        matches!(self.state, MutationJournalState::Completed)
    }
}

impl Storable for GraphMutationJournalEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode GraphMutationJournalEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode GraphMutationJournalEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode GraphMutationJournalEntry")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredLabelStatsDeltaEvent(LabelStatsDeltaEventWire);

impl From<LabelStatsDeltaEventWire> for StoredLabelStatsDeltaEvent {
    fn from(value: LabelStatsDeltaEventWire) -> Self {
        Self(value)
    }
}

impl From<StoredLabelStatsDeltaEvent> for LabelStatsDeltaEventWire {
    fn from(value: StoredLabelStatsDeltaEvent) -> Self {
        value.0
    }
}

impl Storable for StoredLabelStatsDeltaEvent {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(&self.0).expect("encode LabelStatsDeltaEventWire"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self.0).expect("encode LabelStatsDeltaEventWire")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(
            Decode!(bytes.as_ref(), LabelStatsDeltaEventWire)
                .expect("decode LabelStatsDeltaEventWire"),
        )
    }
}

pub struct LabelStatsDeltaLog<M: Memory> {
    map: StableBTreeMap<ShardEventSeq, StoredLabelStatsDeltaEvent, M>,
}

impl<M: Memory> LabelStatsDeltaLog<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(&mut self, event: LabelStatsDeltaEventWire) {
        self.map.insert(event.shard_event_seq, event.into());
    }

    pub fn remove_through(&mut self, through_seq: ShardEventSeq) {
        let to_remove: Vec<ShardEventSeq> = self
            .map
            .range(..=through_seq)
            .map(|entry| *entry.key())
            .collect();
        for seq in to_remove {
            self.map.remove(&seq);
        }
    }

    pub fn list_from(&self, from_seq: ShardEventSeq, limit: u32) -> Vec<LabelStatsDeltaEventWire> {
        let mut out = Vec::new();
        for entry in self.map.range(from_seq..) {
            out.push(entry.value().into());
            if out.len() >= limit as usize {
                break;
            }
        }
        out
    }
}

pub struct GraphMutationJournal<M: Memory> {
    map: StableBTreeMap<MutationId, GraphMutationJournalEntry, M>,
}

impl<M: Memory> GraphMutationJournal<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    #[cfg(feature = "pocket-ic-e2e")]
    pub fn len(&self) -> u64 {
        self.map.len()
    }

    pub fn get(&self, mutation_id: MutationId) -> Option<GraphMutationJournalEntry> {
        self.map.get(&mutation_id)
    }

    pub fn insert(&mut self, entry: GraphMutationJournalEntry) {
        self.map.insert(entry.mutation_id, entry);
    }

    /// One amortized, budgeted retention pass over the keyspace starting strictly after
    /// `start_after` (ADR 0027). Entries older than `retention_ns` are evicted; legacy
    /// entries with no timestamp are lazy-stamped to `now` (so they age out from upgrade
    /// time, never prematurely). Returns `(scanned, removed, last_examined_key)`; the
    /// caller persists `last_examined_key` as the next round-robin cursor.
    pub fn evict_expired(
        &mut self,
        start_after: Option<MutationId>,
        budget: usize,
        now: u64,
        retention_ns: u64,
    ) -> (u32, u32, Option<MutationId>) {
        let lower = match start_after {
            Some(id) => StdBound::Excluded(id),
            None => StdBound::Unbounded,
        };
        let mut scanned: u32 = 0;
        let mut last_key: Option<MutationId> = None;
        let mut to_remove: Vec<MutationId> = Vec::new();
        let mut to_stamp: Vec<GraphMutationJournalEntry> = Vec::new();
        for entry in self.map.range((lower, StdBound::Unbounded)).take(budget) {
            let id = *entry.key();
            let value = entry.value();
            scanned += 1;
            last_key = Some(id);
            match value.recorded_at_ns {
                None => {
                    let mut stamped = value;
                    stamped.recorded_at_ns = Some(now);
                    to_stamp.push(stamped);
                }
                Some(ts) if now.saturating_sub(ts) > retention_ns => to_remove.push(id),
                Some(_) => {}
            }
        }
        let removed = to_remove.len() as u32;
        for id in &to_remove {
            self.map.remove(id);
        }
        for entry in to_stamp {
            self.map.insert(entry.mutation_id, entry);
        }
        (scanned, removed, last_key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::VectorMemory;

    const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;
    const RETENTION_NS: u64 = 9 * DAY_NS;

    fn journal() -> GraphMutationJournal<VectorMemory> {
        GraphMutationJournal::init(VectorMemory::default())
    }

    fn completed_at(mutation_id: MutationId, recorded_at_ns: u64) -> GraphMutationJournalEntry {
        GraphMutationJournalEntry::completed(mutation_id, 1, None, None, Vec::new(), recorded_at_ns)
    }

    #[test]
    fn evicts_completed_entry_past_retention() {
        let mut j = journal();
        j.insert(completed_at(1, 0));
        let (scanned, removed, _) = j.evict_expired(None, 8, RETENTION_NS + 1, RETENTION_NS);
        assert_eq!((scanned, removed), (1, 1));
        assert!(j.get(1).is_none());
    }

    #[test]
    fn retains_entry_within_retention() {
        let mut j = journal();
        j.insert(completed_at(1, 0));
        let (scanned, removed, _) = j.evict_expired(None, 8, RETENTION_NS - 1, RETENTION_NS);
        assert_eq!((scanned, removed), (1, 0));
        assert!(j.get(1).is_some());
    }

    #[test]
    fn evicts_incomplete_entry_past_retention() {
        // Persisted Incomplete entries are also replay-dedup markers, so they are
        // age-bounded too (must not leak, must not be dropped within the replay window).
        let mut j = journal();
        j.insert(GraphMutationJournalEntry::incomplete(2, None, None, 0));
        let (_, removed_early, _) = j.evict_expired(None, 8, RETENTION_NS - 1, RETENTION_NS);
        assert_eq!(removed_early, 0);
        let (_, removed_late, _) = j.evict_expired(None, 8, RETENTION_NS + 1, RETENTION_NS);
        assert_eq!(removed_late, 1);
        assert!(j.get(2).is_none());
    }

    #[test]
    fn lazy_stamps_legacy_entry_then_evicts_after_retention() {
        let mut j = journal();
        // Pre-ADR-0027 bytes decode with no timestamp.
        j.insert(GraphMutationJournalEntry {
            mutation_id: 3,
            state: MutationJournalState::Completed,
            row_count: 1,
            emitted_delta_first_seq: None,
            emitted_delta_last_seq: None,
            hot_forward_vertices: Vec::new(),
            recorded_at_ns: None,
        });
        let upgrade_ns = 1_000 * DAY_NS;
        // First visit lazy-stamps to "now" instead of evicting, even though "now" is huge.
        let (_, removed, _) = j.evict_expired(None, 8, upgrade_ns, RETENTION_NS);
        assert_eq!(removed, 0);
        assert_eq!(j.get(3).unwrap().recorded_at_ns, Some(upgrade_ns));
        // It then ages out from the stamp time, not from epoch.
        let (_, removed_before, _) =
            j.evict_expired(None, 8, upgrade_ns + RETENTION_NS - 1, RETENTION_NS);
        assert_eq!(removed_before, 0);
        let (_, removed_after, _) =
            j.evict_expired(None, 8, upgrade_ns + RETENTION_NS + 1, RETENTION_NS);
        assert_eq!(removed_after, 1);
        assert!(j.get(3).is_none());
    }

    #[test]
    fn round_robin_cursor_evicts_all_across_laps() {
        let mut j = journal();
        for id in 1..=5u64 {
            j.insert(completed_at(id, 0));
        }
        let now = RETENTION_NS + 1;
        let mut cursor = None;
        let mut total_removed = 0u32;
        loop {
            let (scanned, removed, last) = j.evict_expired(cursor, 2, now, RETENTION_NS);
            total_removed += removed;
            if scanned < 2 {
                break;
            }
            cursor = last;
        }
        assert_eq!(total_removed, 5);
        for id in 1..=5u64 {
            assert!(j.get(id).is_none());
        }
    }
}
