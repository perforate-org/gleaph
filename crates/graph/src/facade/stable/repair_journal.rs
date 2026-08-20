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
use gleaph_graph_kernel::index::{IndexMaintenancePhase, PhysicalIndexId};
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
use ic_stable_structures::{Memory, StableBTreeMap, Storable, storable::Bound};
use std::borrow::Cow;

/// One durable posting operation awaiting re-application to graph-index. The
/// three variants mirror the volatile pending queues (`index/pending.rs`,
/// `edge_pending.rs`, `label_pending.rs`); `remove == false` means insert.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum RepairPostingOp {
    VertexProperty {
        physical_index_id: PhysicalIndexId,
        catalog_epoch: u64,
        phase: IndexMaintenancePhase,
        remove: bool,
        property_id: u32,
        payload_bytes: Vec<u8>,
        vertex_id: u32,
    },
    EdgeProperty {
        physical_index_id: PhysicalIndexId,
        catalog_epoch: u64,
        phase: IndexMaintenancePhase,
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
    /// A derived vector-index embedding mutation awaiting re-delivery to `vector-canister`
    /// (ADR 0031). The whole self-describing wire op is journaled; re-application is idempotent
    /// (version-guarded upsert / tombstone), so no compensation is needed on the drain path.
    VectorEmbedding { op: VectorEmbeddingSyncOp },
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

/// One journal entry: the deferred posting op plus the federated `mutation_id` that
/// produced it (ADR 0029 Phase 2). `mutation_id == 0` is the reserved *untracked*
/// sentinel — used for posting work with no federated mutation context (e.g. a
/// maintenance-timer flush of leftover pending postings). Untracked entries still
/// drain in order but never participate in the mutation-linked index floor.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct RepairJournalEntry {
    pub mutation_id: u64,
    pub op: RepairPostingOp,
}

impl Storable for RepairJournalEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode RepairJournalEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode RepairJournalEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), Self).expect("decode RepairJournalEntry")
    }
}

/// Stable FIFO-ish journal keyed by a monotonic sequence so entries replay in the
/// order they failed. Entries are removed individually as the index accepts them.
pub struct RepairJournal<M: Memory> {
    map: StableBTreeMap<u64, RepairJournalEntry, M>,
}

impl<M: Memory> RepairJournal<M> {
    pub fn init(memory: M) -> Self {
        Self {
            map: StableBTreeMap::init(memory),
        }
    }

    fn next_seq(&self) -> Result<u64, &'static str> {
        self.map.last_key_value().map_or(Ok(0), |(seq, _)| {
            seq.checked_add(1).ok_or("repair journal sequence overflow")
        })
    }

    /// Appends `ops` in order under one `mutation_id`, preserving their relative
    /// sequence. Pass `mutation_id == 0` for posting work with no federated mutation
    /// context (see [`RepairJournalEntry`]).
    pub fn prepare_append_all(
        &self,
        mutation_id: u64,
        ops: impl IntoIterator<Item = RepairPostingOp>,
    ) -> Result<Vec<(u64, RepairJournalEntry)>, &'static str> {
        let ops: Vec<_> = ops.into_iter().collect();
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let first = self.next_seq()?;
        ops.into_iter()
            .enumerate()
            .map(|(offset, op)| {
                let offset = u64::try_from(offset).map_err(|_| "repair journal batch overflow")?;
                let sequence = first
                    .checked_add(offset)
                    .ok_or("repair journal sequence overflow")?;
                let entry = RepairJournalEntry { mutation_id, op };
                let _ = entry.to_bytes();
                Ok((sequence, entry))
            })
            .collect()
    }

    pub fn append_prepared(
        &mut self,
        rows: Vec<(u64, RepairJournalEntry)>,
    ) -> Vec<(u64, RepairJournalEntry)> {
        for (sequence, entry) in &rows {
            assert!(
                self.map.insert(*sequence, entry.clone()).is_none(),
                "duplicate repair journal sequence"
            );
        }
        rows
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
            .map(|entry| (*entry.key(), entry.value().op))
            .collect()
    }

    pub fn get(&self, sequence: u64) -> Option<RepairJournalEntry> {
        self.map.get(&sequence)
    }

    pub fn remove(&mut self, sequence: u64) -> Option<(u64, RepairJournalEntry)> {
        self.map.remove(&sequence).map(|entry| (sequence, entry))
    }

    #[cfg(test)]
    pub fn sequences(&self) -> Vec<u64> {
        self.map.iter().map(|row| *row.key()).collect()
    }
}
