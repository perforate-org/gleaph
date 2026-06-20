//! Durable repair journal for federated index postings (ADR 0023 D5).
//!
//! The happy-path posting flush stays volatile and persists nothing. When a
//! flush fails *after its compensation succeeds* (the index is back at its
//! pre-batch state), the failed batch is appended here — to stable memory — so
//! the store-ahead/index-behind delta survives the upgrade boundary, the timer
//! context, and traps. The maintenance driver re-applies journal ops on each
//! tick and on `post_upgrade`, removing each entry once the index canister
//! accepts it. Re-application is idempotent (graph-index `remove` is a no-op on a
//! missing key and `insert` sets membership), so no compensation is needed on the
//! drain path.

use candid::{Decode, Encode};
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

/// One durable posting operation awaiting re-application to graph-index. The
/// three variants mirror the volatile pending queues (`index/pending.rs`,
/// `edge_pending.rs`, `label_pending.rs`); `remove == false` means insert.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum RepairPostingOp {
    VertexProperty {
        remove: bool,
        property_id: u32,
        payload_bytes: Vec<u8>,
        vertex_id: u32,
    },
    EdgeProperty {
        remove: bool,
        property_id: u32,
        payload_bytes: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    },
    Label {
        remove: bool,
        label_id: u32,
        vertex_id: u32,
    },
}

impl Storable for RepairPostingOp {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode RepairPostingOp"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode RepairPostingOp")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode RepairPostingOp")
    }
}

/// Stable FIFO-ish journal keyed by a monotonic sequence so entries replay in the
/// order they failed. Entries are removed individually as the index accepts them.
pub struct RepairJournal<M: Memory> {
    map: StableBTreeMap<u64, RepairPostingOp, M>,
}

impl<M: Memory> RepairJournal<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    fn next_seq(&self) -> u64 {
        self.map.last_key_value().map_or(0, |(seq, _)| seq + 1)
    }

    /// Appends `ops` in order, preserving their relative sequence.
    pub fn append_all(&mut self, ops: impl IntoIterator<Item = RepairPostingOp>) {
        for (seq, op) in (self.next_seq()..).zip(ops) {
            self.map.insert(seq, op);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> u64 {
        self.map.len()
    }

    /// Reads up to `limit` oldest entries (sequence-ordered) for re-application.
    pub fn peek(&self, limit: usize) -> Vec<(u64, RepairPostingOp)> {
        self.map
            .iter()
            .take(limit)
            .map(|entry| (*entry.key(), entry.value()))
            .collect()
    }

    pub fn remove(&mut self, seq: u64) {
        self.map.remove(&seq);
    }
}
