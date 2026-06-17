//! Stable label stats delta log and graph mutation journal (ADR 0015).

use candid::{Decode, Encode};
use gleaph_graph_kernel::federation::LocalVertexId;
use gleaph_graph_kernel::plan_exec::{
    GraphMutationJournalEntryWire, LabelStatsDeltaEventWire, MutationId, MutationJournalState,
    ShardEventSeq,
};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct GraphMutationJournalEntry {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub row_count: u64,
    pub emitted_delta_first_seq: Option<ShardEventSeq>,
    pub emitted_delta_last_seq: Option<ShardEventSeq>,
    pub hot_forward_vertices: Vec<LocalVertexId>,
}

impl GraphMutationJournalEntry {
    pub fn incomplete(
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
    ) -> Self {
        Self {
            mutation_id,
            state: MutationJournalState::Incomplete,
            row_count: 0,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: Vec::new(),
        }
    }

    pub fn completed(
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) -> Self {
        Self {
            mutation_id,
            state: MutationJournalState::Completed,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
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

    pub fn get(&self, mutation_id: MutationId) -> Option<GraphMutationJournalEntry> {
        self.map.get(&mutation_id)
    }

    pub fn insert(&mut self, entry: GraphMutationJournalEntry) {
        self.map.insert(entry.mutation_id, entry);
    }
}
