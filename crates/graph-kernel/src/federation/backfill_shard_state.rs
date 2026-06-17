//! Packed router-stable cursor for shard-local posting backfill.

use super::LocalVertexId;
use candid::CandidType;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Local vertex ids use 30 payload bits (see `entry::vertex_ref::VertexRef` encoding).
const LOCAL_VERTEX_ID_MASK: u32 = (1 << 30) - 1;
const DONE_BIT: u32 = 1 << 30;

/// Router-stable progress cursor for one shard posting backfill (label or vertex property).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct BackfillShardState {
    pub next_vertex_id: LocalVertexId,
    pub done: bool,
}

impl BackfillShardState {
    pub fn apply_batch_progress(&mut self, next_vertex_id: LocalVertexId, done: bool) {
        self.next_vertex_id = next_vertex_id;
        self.done = done;
    }

    fn pack(self) -> u32 {
        let id = self.next_vertex_id & LOCAL_VERTEX_ID_MASK;
        if self.done { id | DONE_BIT } else { id }
    }

    fn unpack(raw: u32) -> Self {
        Self {
            next_vertex_id: raw & LOCAL_VERTEX_ID_MASK,
            done: raw & DONE_BIT != 0,
        }
    }
}

impl Storable for BackfillShardState {
    const BOUND: Bound = Bound::Bounded {
        max_size: 4,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.pack().to_le_bytes().into()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::unpack(u32::from_le_bytes(
            bytes.as_ref().try_into().expect("4 bytes"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrips_done_and_vertex_id() {
        let state = BackfillShardState {
            next_vertex_id: LOCAL_VERTEX_ID_MASK,
            done: true,
        };
        assert_eq!(BackfillShardState::unpack(state.pack()), state);
        assert_eq!(state.into_bytes(), state.pack().to_le_bytes().to_vec());
    }
}
