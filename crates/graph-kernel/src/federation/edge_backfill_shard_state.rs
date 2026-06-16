//! Packed router-stable cursor for shard-local edge property posting backfill.

use candid::CandidType;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Lexicographic resume cursor on graph shard [`EdgePropertyKey`] wire bytes.
pub const EDGE_PROPERTY_KEY_BYTES: usize = 14;

const STORED_SIZE: usize = 1 + EDGE_PROPERTY_KEY_BYTES;
const DONE_BIT: u8 = 0x01;
const HAS_AFTER_KEY_BIT: u8 = 0x02;

/// Router-stable progress cursor for one shard edge posting backfill.
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct EdgeBackfillShardState {
    pub after_key: Option<Vec<u8>>,
    pub done: bool,
}

impl EdgeBackfillShardState {
    pub fn apply_batch_progress(&mut self, next_after_key: Option<Vec<u8>>, done: bool) {
        if let Some(ref key) = next_after_key {
            debug_assert_eq!(key.len(), EDGE_PROPERTY_KEY_BYTES);
        }
        self.after_key = next_after_key;
        self.done = done;
    }

    fn pack(&self) -> [u8; STORED_SIZE] {
        let mut out = [0u8; STORED_SIZE];
        let mut flags = 0u8;
        if self.done {
            flags |= DONE_BIT;
        }
        if let Some(ref key) = self.after_key {
            flags |= HAS_AFTER_KEY_BIT;
            out[1..STORED_SIZE].copy_from_slice(key);
        }
        out[0] = flags;
        out
    }

    fn unpack(bytes: &[u8; STORED_SIZE]) -> Self {
        let flags = bytes[0];
        let done = flags & DONE_BIT != 0;
        let after_key = if flags & HAS_AFTER_KEY_BIT != 0 {
            Some(bytes[1..STORED_SIZE].to_vec())
        } else {
            None
        };
        Self { after_key, done }
    }
}

impl Storable for EdgeBackfillShardState {
    const BOUND: Bound = Bound::Bounded {
        max_size: STORED_SIZE as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.pack().into())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.pack().into()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::unpack(
            bytes
                .as_ref()
                .try_into()
                .expect("edge backfill shard state size"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrips_done_and_after_key() {
        let key = vec![1u8; EDGE_PROPERTY_KEY_BYTES];
        let state = EdgeBackfillShardState {
            after_key: Some(key.clone()),
            done: true,
        };
        let bytes = state.clone().into_bytes();
        assert_eq!(EdgeBackfillShardState::from_bytes(Cow::Owned(bytes)), state);
    }

    #[test]
    fn default_cursor_has_no_after_key() {
        let state = EdgeBackfillShardState::default();
        assert!(!state.done);
        assert!(state.after_key.is_none());
    }
}
