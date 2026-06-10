//! Router-owned cursor state for property posting backfill per shard.

use candid::CandidType;
use gleaph_graph_kernel::federation::LocalVertexId;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PropertyBackfillShardState {
    pub next_vertex_id: LocalVertexId,
    pub done: bool,
}

impl Storable for PropertyBackfillShardState {
    const BOUND: Bound = Bound::Bounded {
        max_size: 5,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5);
        out.extend_from_slice(&self.next_vertex_id.to_le_bytes());
        out.push(u8::from(self.done));
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut raw = [0; 4];
        raw.copy_from_slice(&bytes[0..4]);
        Self {
            next_vertex_id: u32::from_le_bytes(raw),
            done: bytes[4] != 0,
        }
    }
}
