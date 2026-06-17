//! Composite registry key for graph-local shard identity (ADR 0019).

use crate::entry::GraphId;

use candid::CandidType;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use super::ShardId;

/// Router registry key: shard ordinals are unique per [`GraphId`], not federation-wide.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct GraphShardKey {
    pub graph_id: GraphId,
    pub shard_id: ShardId,
}

impl GraphShardKey {
    #[inline]
    pub const fn new(graph_id: GraphId, shard_id: ShardId) -> Self {
        Self { graph_id, shard_id }
    }
}

impl Storable for GraphShardKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&self.graph_id.to_le_bytes());
        out.extend_from_slice(&self.shard_id.to_le_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut graph = [0; 4];
        let mut shard = [0; 4];
        graph.copy_from_slice(&bytes[0..4]);
        shard.copy_from_slice(&bytes[4..8]);
        Self {
            graph_id: GraphId::from_le_bytes(graph),
            shard_id: ShardId::from_le_bytes(shard),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::Storable;

    #[test]
    fn graph_shard_key_storable_roundtrip() {
        let key = GraphShardKey::new(GraphId::from_raw(3), ShardId::new(0));
        assert_eq!(key, GraphShardKey::from_bytes(key.to_bytes()));
    }

    #[test]
    fn same_shard_ordinal_differs_by_graph() {
        let a = GraphShardKey::new(GraphId::from_raw(1), ShardId::new(0));
        let b = GraphShardKey::new(GraphId::from_raw(2), ShardId::new(0));
        assert_ne!(a, b);
        assert_ne!(a.to_bytes(), b.to_bytes());
    }
}
