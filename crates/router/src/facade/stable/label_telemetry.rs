//! Router-owned label telemetry records.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{
    LabelTelemetryEventWire, MutationId, ResolvedLabelTable, ResolvedPropertyTable,
};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LabelStats {
    pub live_count: u64,
    pub total_adds: u64,
    pub total_removes: u64,
}

impl Storable for LabelStats {
    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        out.extend_from_slice(&self.live_count.to_le_bytes());
        out.extend_from_slice(&self.total_adds.to_le_bytes());
        out.extend_from_slice(&self.total_removes.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut live = [0; 8];
        let mut adds = [0; 8];
        let mut removes = [0; 8];
        live.copy_from_slice(&bytes[0..8]);
        adds.copy_from_slice(&bytes[8..16]);
        removes.copy_from_slice(&bytes[16..24]);
        Self {
            live_count: u64::from_le_bytes(live),
            total_adds: u64::from_le_bytes(adds),
            total_removes: u64::from_le_bytes(removes),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LabelShardKey {
    pub shard_id: ShardId,
    pub label_id: u16,
}

impl LabelShardKey {
    pub const fn new(shard_id: ShardId, label_id: u16) -> Self {
        Self { shard_id, label_id }
    }
}

impl Storable for LabelShardKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut shard = [0; 4];
        let mut label = [0; 2];
        shard.copy_from_slice(&bytes[0..4]);
        label.copy_from_slice(&bytes[4..6]);
        Self {
            shard_id: ShardId::from_le_bytes(shard),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AppliedLabelTelemetryKey {
    pub shard_id: ShardId,
    pub shard_event_seq: u64,
}

impl AppliedLabelTelemetryKey {
    pub const fn new(shard_id: ShardId, shard_event_seq: u64) -> Self {
        Self {
            shard_id,
            shard_event_seq,
        }
    }
}

impl Storable for AppliedLabelTelemetryKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 12,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12);
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.shard_event_seq.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut shard = [0; 4];
        let mut seq = [0; 8];
        shard.copy_from_slice(&bytes[0..4]);
        seq.copy_from_slice(&bytes[4..12]);
        Self {
            shard_id: ShardId::from_le_bytes(shard),
            shard_event_seq: u64::from_le_bytes(seq),
        }
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientMutationKey {
    pub caller: Principal,
    pub logical_graph_name: String,
    pub client_key: String,
}

impl ClientMutationKey {
    pub fn new(caller: Principal, logical_graph_name: String, client_key: String) -> Self {
        Self {
            caller,
            logical_graph_name,
            client_key,
        }
    }
}

impl Storable for ClientMutationKey {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ClientMutationKey"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ClientMutationKey")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode ClientMutationKey")
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterMutationRecord {
    pub mutation_id: MutationId,
    pub created_at_ns: u64,
    pub request_fingerprint: Vec<u8>,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub completed_row_count: Option<u64>,
    pub routing_in_progress: bool,
    pub shards: Vec<RouterMutationShard>,
}

impl RouterMutationRecord {
    pub fn new(mutation_id: MutationId, created_at_ns: u64, request_fingerprint: Vec<u8>) -> Self {
        Self {
            mutation_id,
            created_at_ns,
            request_fingerprint,
            resolved_labels: None,
            resolved_properties: None,
            completed_row_count: None,
            routing_in_progress: true,
            shards: Vec::new(),
        }
    }
}

impl Storable for RouterMutationRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode RouterMutationRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode RouterMutationRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode RouterMutationRecord")
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterMutationShard {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub seed_bindings_blob: Option<Vec<u8>>,
    pub completed: bool,
    pub telemetry_acked: bool,
    pub row_count: u64,
    pub label_telemetry_events: Vec<LabelTelemetryEventWire>,
}

impl RouterMutationShard {
    pub fn new(
        shard_id: ShardId,
        graph_canister: Principal,
        seed_bindings_blob: Option<Vec<u8>>,
    ) -> Self {
        Self {
            shard_id,
            graph_canister,
            seed_bindings_blob,
            completed: false,
            telemetry_acked: false,
            row_count: 0,
            label_telemetry_events: Vec::new(),
        }
    }
}
