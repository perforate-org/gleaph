//! Stable outbox for shard-local label telemetry events.

use candid::{Decode, Encode};
use gleaph_graph_kernel::plan_exec::{
    LabelTelemetryEventWire, MutationId, MutationOutcomeWire, ShardEventSeq,
};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct AppliedMutationRequest {
    pub mutation_id: MutationId,
    pub completed: bool,
    pub row_count: u64,
    pub label_telemetry_events: Vec<LabelTelemetryEventWire>,
}

impl AppliedMutationRequest {
    pub fn incomplete(
        mutation_id: MutationId,
        label_telemetry_events: Vec<LabelTelemetryEventWire>,
    ) -> Self {
        Self {
            mutation_id,
            completed: false,
            row_count: 0,
            label_telemetry_events,
        }
    }

    pub fn completed(
        mutation_id: MutationId,
        row_count: u64,
        label_telemetry_events: Vec<LabelTelemetryEventWire>,
    ) -> Self {
        Self {
            mutation_id,
            completed: true,
            row_count,
            label_telemetry_events,
        }
    }
}

impl From<AppliedMutationRequest> for MutationOutcomeWire {
    fn from(value: AppliedMutationRequest) -> Self {
        Self {
            mutation_id: value.mutation_id,
            completed: value.completed,
            row_count: value.row_count,
            label_telemetry_events: value.label_telemetry_events,
        }
    }
}

impl Storable for AppliedMutationRequest {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode AppliedMutationRequest"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode AppliedMutationRequest")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode AppliedMutationRequest")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredLabelTelemetryEvent(LabelTelemetryEventWire);

impl From<LabelTelemetryEventWire> for StoredLabelTelemetryEvent {
    fn from(value: LabelTelemetryEventWire) -> Self {
        Self(value)
    }
}

impl From<StoredLabelTelemetryEvent> for LabelTelemetryEventWire {
    fn from(value: StoredLabelTelemetryEvent) -> Self {
        value.0
    }
}

impl Storable for StoredLabelTelemetryEvent {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(&self.0).expect("encode LabelTelemetryEventWire"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self.0).expect("encode LabelTelemetryEventWire")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(
            Decode!(bytes.as_ref(), LabelTelemetryEventWire)
                .expect("decode LabelTelemetryEventWire"),
        )
    }
}

pub struct LabelTelemetryOutbox<M: Memory> {
    map: StableBTreeMap<ShardEventSeq, StoredLabelTelemetryEvent, M>,
}

impl<M: Memory> LabelTelemetryOutbox<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn insert(&mut self, event: LabelTelemetryEventWire) {
        self.map.insert(event.shard_event_seq, event.into());
    }

    pub fn get(&self, seq: ShardEventSeq) -> Option<LabelTelemetryEventWire> {
        self.map.get(&seq).map(Into::into)
    }

    pub fn remove(&mut self, seq: ShardEventSeq) {
        self.map.remove(&seq);
    }

    pub fn list_from(&self, from_seq: ShardEventSeq, limit: u32) -> Vec<LabelTelemetryEventWire> {
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

pub struct AppliedMutationRequests<M: Memory> {
    map: StableBTreeMap<MutationId, AppliedMutationRequest, M>,
}

impl<M: Memory> AppliedMutationRequests<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    pub fn get(&self, mutation_id: MutationId) -> Option<AppliedMutationRequest> {
        self.map.get(&mutation_id)
    }

    pub fn insert(&mut self, request: AppliedMutationRequest) {
        self.map.insert(request.mutation_id, request);
    }
}
