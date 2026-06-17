//! Router-owned label stats aggregates and client mutation records (ADR 0015).

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::plan_exec::{MutationId, ResolvedLabelTable, ResolvedPropertyTable};
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
pub struct GraphLabelKey {
    pub graph_id: GraphId,
    pub label_id: u16,
}

impl GraphLabelKey {
    pub const fn new(graph_id: GraphId, label_id: u16) -> Self {
        Self { graph_id, label_id }
    }
}

impl Storable for GraphLabelKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 6,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut label = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        label.copy_from_slice(&bytes[4..6]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphLabelShardKey {
    pub graph_id: GraphId,
    pub shard_id: ShardId,
    pub label_id: u16,
}

impl GraphLabelShardKey {
    pub const fn new(graph_id: GraphId, shard_id: ShardId, label_id: u16) -> Self {
        Self {
            graph_id,
            shard_id,
            label_id,
        }
    }
}

impl Storable for GraphLabelShardKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 10,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(10);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out.extend_from_slice(&self.label_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut shard = [0; 4];
        let mut label = [0; 2];
        graph.copy_from_slice(&bytes[0..4]);
        shard.copy_from_slice(&bytes[4..8]);
        label.copy_from_slice(&bytes[8..10]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            shard_id: ShardId::from_le_bytes(shard),
            label_id: u16::from_le_bytes(label),
        }
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClientMutationKey {
    pub caller: Principal,
    pub graph_id: GraphId,
    pub client_key: String,
}

impl ClientMutationKey {
    pub fn new(caller: Principal, graph_id: GraphId, client_key: String) -> Self {
        Self {
            caller,
            graph_id,
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

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
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

/// Stable-memory wire envelope for [`RouterMutationRecord`].
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq)]
enum RouterMutationStableRecord {
    V1(RouterMutationRecord),
}

impl Storable for RouterMutationRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&RouterMutationStableRecord::V1(self.clone()))
                .expect("encode RouterMutationRecord"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&RouterMutationStableRecord::V1(self)).expect("encode RouterMutationRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), RouterMutationStableRecord)
            .expect("decode RouterMutationRecord")
        {
            RouterMutationStableRecord::V1(v1) => v1,
        }
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RouterMutationShard {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub seed_bindings_blob: Option<Vec<u8>>,
    pub completed: bool,
    pub projection_advanced: bool,
    pub row_count: u64,
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
            projection_advanced: false,
            row_count: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::Storable;

    #[test]
    fn router_mutation_record_round_trips_through_storable() {
        let record = RouterMutationRecord::new(1, 42, vec![9, 8]);
        let decoded = RouterMutationRecord::from_bytes(Cow::Owned(record.clone().into_bytes()));
        assert_eq!(decoded, record);
        assert_eq!(decoded.mutation_id, 1);
        assert!(decoded.routing_in_progress);
    }
}
