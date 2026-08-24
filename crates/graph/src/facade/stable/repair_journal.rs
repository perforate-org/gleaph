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

/// One journal entry: the deferred posting op plus the federated `mutation_id` that
/// produced it (ADR 0029 Phase 2). `mutation_id == 0` is the reserved *untracked*
/// sentinel — used for posting work with no federated mutation context (e.g. a
/// maintenance-timer flush of leftover pending postings). Untracked entries still
/// drain in order but never participate in the mutation-linked index floor.
///
/// Rows persist through [`RepairJournalStableRecord::V1`]: record-shape revisions add a new
/// envelope variant instead of replacing bytes in place (Plan 0302; registry compat is
/// `ProductionCompat::VersionedSurvivor`).
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct RepairJournalEntry {
    pub mutation_id: u64,
    pub op: RepairPostingOp,
}

/// Versioned Candid wire envelope for one durable MemoryId 41 journal row.
///
/// Persisted rows always carry this outer tag so a future record-shape revision is wire-additive
/// (a new variant) instead of panicking decode of rows that span upgrade windows. Store methods
/// keep taking and returning bare [`RepairJournalEntry`] values; the envelope is constructed and
/// unwrapped only inside this file's stable-storage paths.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum RepairJournalStableRecord {
    V1(RepairJournalEntry),
}

impl Storable for RepairJournalStableRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode RepairJournalEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode RepairJournalEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), Self).expect("decode RepairJournalEntry") {
            // Exhaustive over the envelope: a future variant forces an explicit decision here,
            // and foreign Candid payloads fail closed through the expect above.
            v1 @ Self::V1(_) => v1,
        }
    }
}

fn to_envelope(entry: RepairJournalEntry) -> RepairJournalStableRecord {
    RepairJournalStableRecord::V1(entry)
}

fn from_envelope(envelope: RepairJournalStableRecord) -> RepairJournalEntry {
    match envelope {
        RepairJournalStableRecord::V1(entry) => entry,
    }
}

/// Stable FIFO-ish journal keyed by a monotonic sequence so entries replay in the
/// order they failed. Entries are removed individually as the index accepts them.
pub struct RepairJournal<M: Memory> {
    map: StableBTreeMap<u64, RepairJournalStableRecord, M>,
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
                let _ = to_envelope(entry.clone()).to_bytes();
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
                self.map
                    .insert(*sequence, to_envelope(entry.clone()))
                    .is_none(),
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
            .map(|row| (*row.key(), from_envelope(row.value()).op))
            .collect()
    }

    pub fn get(&self, sequence: u64) -> Option<RepairJournalEntry> {
        self.map.get(&sequence).map(from_envelope)
    }

    pub fn remove(&mut self, sequence: u64) -> Option<(u64, RepairJournalEntry)> {
        self.map
            .remove(&sequence)
            .map(|envelope| (sequence, from_envelope(envelope)))
    }

    #[cfg(test)]
    pub fn sequences(&self) -> Vec<u64> {
        self.map.iter().map(|row| *row.key()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{VectorEncoding, VectorMetric, VectorSubject};
    use ic_stable_structures::VectorMemory;

    /// One representative op per [`RepairPostingOp`] variant, mirroring the fixture shapes the
    /// pending-queue conversion paths (`index/pending.rs`, `vector_pending.rs`) journal today.
    fn vertex_property_op(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VertexProperty {
            physical_index_id: PhysicalIndexId::new(900_101).expect("test physical id"),
            catalog_epoch: 3,
            phase: IndexMaintenancePhase::Active,
            remove: false,
            property_id: 7,
            payload_bytes: vec![1, 2, 3],
            vertex_id,
        }
    }

    fn edge_property_op() -> RepairPostingOp {
        RepairPostingOp::EdgeProperty {
            physical_index_id: PhysicalIndexId::new(900_102).expect("test physical id"),
            catalog_epoch: 4,
            phase: IndexMaintenancePhase::Active,
            remove: true,
            property_id: 9,
            payload_bytes: vec![4, 5],
            label_id: 11,
            owner_vertex_id: 42,
            slot_index: 5,
        }
    }

    fn label_op(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::Label {
            remove: false,
            label_id: 13,
            vertex_id,
        }
    }

    fn vector_embedding_op(vertex_id: u32) -> RepairPostingOp {
        RepairPostingOp::VectorEmbedding {
            op: VectorEmbeddingSyncOp {
                index_id: 1,
                embedding_name_id: 2,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id,
                },
                mutation_id: 100 + u64::from(vertex_id),
                encoding: VectorEncoding::F32,
                dims: 1,
                metric: VectorMetric::L2Squared,
                bytes: vec![0xa0, vertex_id as u8, 0x5a, 0xff - vertex_id as u8],
                remove: false,
            },
        }
    }

    #[test]
    fn all_four_op_variants_round_trip_across_stable_reopen() {
        let memory = VectorMemory::default();
        let mut journal = RepairJournal::init(memory.clone());
        let ops = [
            vertex_property_op(1),
            edge_property_op(),
            label_op(2),
            vector_embedding_op(3),
        ];
        let rows = journal
            .prepare_append_all(77, ops.clone())
            .expect("prepare four-variant batch");
        assert_eq!(rows.len(), 4);
        let expected: Vec<_> = ops
            .iter()
            .enumerate()
            .map(|(offset, op)| {
                (
                    offset as u64,
                    RepairJournalEntry {
                        mutation_id: 77,
                        op: op.clone(),
                    },
                )
            })
            .collect();
        assert_eq!(rows, expected);
        journal.append_prepared(rows);

        // The outer tag comes first on the wire: rows persist as the enveloped record, never
        // as a bare pre-envelope entry payload.
        let representative = RepairJournalStableRecord::V1(expected[3].1.clone());
        let bytes = Storable::into_bytes(representative.clone());
        assert_eq!(
            Decode!(bytes.as_ref(), RepairJournalStableRecord).expect("decode RepairJournalEntry"),
            representative,
            "durable rows must persist behind the V1 envelope tag"
        );

        // Every entry survives reopen byte-for-byte through real stable storage.
        drop(journal);
        let reopened = RepairJournal::init(memory);
        for (sequence, entry) in &expected {
            assert_eq!(reopened.get(*sequence), Some(entry.clone()));
        }
        assert_eq!(reopened.sequences(), [0, 1, 2, 3]);
    }

    #[test]
    fn peek_returns_ops_in_sequence_order_after_reopen() {
        let memory = VectorMemory::default();
        let mut journal = RepairJournal::init(memory.clone());
        let ops = [
            vertex_property_op(1),
            edge_property_op(),
            label_op(2),
            vector_embedding_op(3),
        ];
        let rows = journal
            .prepare_append_all(77, ops.clone())
            .expect("prepare batch");
        let sequences: Vec<u64> = rows.iter().map(|(seq, _)| *seq).collect();
        journal.append_prepared(rows);

        drop(journal);
        let mut reopened = RepairJournal::init(memory);
        let ack_target = sequences[1];
        assert_eq!(
            reopened.peek(10),
            sequences.into_iter().zip(ops).collect::<Vec<_>>()
        );

        // Drain removes exactly the acknowledged rows and keeps the rest replayable.
        let (acked_seq, _) = reopened.remove(ack_target).expect("acked row exists");
        assert_eq!(acked_seq, ack_target);
        assert_eq!(
            reopened.peek(10).len(),
            3,
            "remaining entries stay replayable after one ack"
        );
        assert!(!reopened.is_empty());
        assert_eq!(reopened.len(), 3);
    }

    #[test]
    #[should_panic(expected = "decode RepairJournalEntry")]
    fn foreign_posting_op_bytes_rejected_by_record_decode() {
        // Encoded bare RepairPostingOp bytes (the pre-envelope wire shape) are foreign to the
        // enveloped record layout and must fail closed instead of decoding into any variant.
        let foreign = Encode!(&label_op(9)).expect("encode foreign posting op");
        drop(RepairJournalStableRecord::from_bytes(Cow::Owned(foreign)));
    }
}
