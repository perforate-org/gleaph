//! INV oracle: graph-index postings == projection of the store over indexed
//! properties (ADR 0023 §"Invariant under review", verification items 1–2).
//!
//! P5 noted INV had no test oracle. This module supplies one: a [`RecordingIndex`]
//! that maintains the *actual* posting set (vertex / edge / label) the way
//! graph-index does (a `BTreeSet` per kind: insert adds a key, remove deletes it,
//! both idempotent). A scenario drives posting ops through the real
//! `pending` / `edge_pending` / `label_pending` flush paths — including a
//! mid-batch index failure that exercises batch-atomic compensation + the durable
//! repair journal — then drains the journal and asserts the recorded posting set
//! equals the intended projection exactly (no missing, no orphan postings).

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

use async_trait::async_trait;
use candid::Principal;
use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    IndexIntersectionRequest, IndexIntersectionResult, PostingHit, PostingRangeRequest,
};
use ic_stable_lara::VertexId;

use crate::facade::{FederationRouting, GraphStore};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::repair_journal::drain_once;
use crate::index::{edge_pending, label_pending, pending};
use crate::plan::PlanQueryError;
use crate::property::PropertyIndexOp;

type VertexPosting = (u32, Vec<u8>, u32);
type EdgePosting = (u32, Vec<u8>, u16, u32, u32);
type LabelPosting = (u32, u32);

#[derive(Default, Clone, PartialEq, Eq, Debug)]
struct IndexProjection {
    vertex: BTreeSet<VertexPosting>,
    edge: BTreeSet<EdgePosting>,
    label: BTreeSet<LabelPosting>,
}

/// In-memory stand-in for graph-index that records the live posting set with the
/// same set semantics (idempotent insert/remove) and can fail the Nth mutating
/// call exactly once to drive the failure path.
struct RecordingIndex {
    state: RefCell<IndexProjection>,
    calls: Cell<usize>,
    fail_at: Cell<Option<usize>>,
}

impl RecordingIndex {
    fn new() -> Self {
        Self {
            state: RefCell::new(IndexProjection::default()),
            calls: Cell::new(0),
            fail_at: Cell::new(None),
        }
    }

    /// Fail the `nth` mutating call (1-based) exactly once, then heal.
    fn fail_once_on(nth: usize) -> Self {
        let index = Self::new();
        index.fail_at.set(Some(nth));
        index
    }

    /// Increments the mutating-call counter; returns `Err` (once) when the count
    /// hits the injected failure point, leaving the recorded state unchanged.
    fn tick(&self) -> Result<(), PlanQueryError> {
        let n = self.calls.get() + 1;
        self.calls.set(n);
        if self.fail_at.get() == Some(n) {
            self.fail_at.set(None);
            return Err(PlanQueryError::UnsupportedOp("inv_oracle_injected_failure"));
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl PropertyIndexLookup for RecordingIndex {
    async fn lookup_equal(
        &self,
        _property_id: u32,
        _value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Ok(vec![])
    }

    async fn lookup_range(
        &self,
        _property_id: u32,
        _req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Ok(vec![])
    }

    async fn lookup_intersection(
        &self,
        _req: &IndexIntersectionRequest,
    ) -> Result<IndexIntersectionResult, PlanQueryError> {
        Ok(IndexIntersectionResult::Vertices(vec![]))
    }

    fn local_shard_id(&self) -> ShardId {
        ShardId::new(0)
    }

    async fn posting_insert_at(
        &self,
        _shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.tick()?;
        self.state
            .borrow_mut()
            .vertex
            .insert((property_id, value, vertex_id));
        Ok(())
    }

    async fn posting_remove_at(
        &self,
        _shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.tick()?;
        self.state
            .borrow_mut()
            .vertex
            .remove(&(property_id, value, vertex_id));
        Ok(())
    }

    async fn label_posting_insert_at(
        &self,
        _shard_id: ShardId,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.tick()?;
        self.state.borrow_mut().label.insert((label_id, vertex_id));
        Ok(())
    }

    async fn label_posting_remove_at(
        &self,
        _shard_id: ShardId,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.tick()?;
        self.state.borrow_mut().label.remove(&(label_id, vertex_id));
        Ok(())
    }

    async fn edge_posting_insert_at(
        &self,
        _shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), PlanQueryError> {
        self.tick()?;
        self.state.borrow_mut().edge.insert((
            property_id,
            value,
            label_id,
            owner_vertex_id,
            slot_index,
        ));
        Ok(())
    }

    async fn edge_posting_remove_at(
        &self,
        _shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), PlanQueryError> {
        self.tick()?;
        self.state.borrow_mut().edge.remove(&(
            property_id,
            value,
            label_id,
            owner_vertex_id,
            slot_index,
        ));
        Ok(())
    }
}

const AGE_PID: u32 = 1;
const WEIGHT_PID: u32 = 2;
const KNOWS_LABEL: u16 = 5;
const OWNER: u32 = 1;

fn vertex_insert(property_id: u32, payload: &[u8]) -> PropertyIndexOp {
    PropertyIndexOp::Insert {
        property_id: PropertyId::from_raw(property_id),
        payload_bytes: payload.to_vec(),
    }
}

fn edge_insert(payload: &[u8]) -> PropertyIndexOp {
    PropertyIndexOp::Insert {
        property_id: PropertyId::from_raw(WEIGHT_PID),
        payload_bytes: payload.to_vec(),
    }
}

fn edge_remove(payload: &[u8]) -> PropertyIndexOp {
    PropertyIndexOp::Remove {
        property_id: PropertyId::from_raw(WEIGHT_PID),
        payload_bytes: payload.to_vec(),
    }
}

/// Runs `body` with federation routing configured and the durable repair journal
/// drained before and after, so the shared thread-local stable state does not
/// leak across tests.
fn with_routing<R>(body: impl FnOnce(&GraphStore) -> R) -> R {
    let graph = GraphStore::new();
    graph
        .set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            shard_id: ShardId::new(0),
        }))
        .expect("set routing");
    drain_journal(&graph);
    pending::clear_pending();
    edge_pending::clear_pending();
    label_pending::clear_pending();
    let out = body(&graph);
    drain_journal(&graph);
    pending::clear_pending();
    edge_pending::clear_pending();
    label_pending::clear_pending();
    graph.set_federation_routing(None).expect("clear routing");
    out
}

fn drain_journal(graph: &GraphStore) {
    for (seq, _) in graph.repair_journal_peek(usize::MAX) {
        graph.repair_journal_remove(seq);
    }
}

/// INV holds after a mutation sequence (incl. an edge compaction re-key) even
/// when a mid-batch index failure forces compensation + durable-journal recovery.
#[test]
fn postings_converge_to_store_projection_after_failure_and_compaction() {
    with_routing(|graph| {
        // The index fails the 2nd mutating call once: it lands inside the first
        // (vertex) batch, forcing batch-atomic compensation + journaling.
        let index = RecordingIndex::fail_once_on(2);

        // Batch 1 — vertex property postings (age). The 2nd insert fails, so the
        // whole batch is compensated to empty and persisted to the repair journal.
        pending::push_vertex_index_op(VertexId::from(1u32), vertex_insert(AGE_PID, &[10]));
        pending::push_vertex_index_op(VertexId::from(2u32), vertex_insert(AGE_PID, &[11]));
        pending::push_vertex_index_op(VertexId::from(3u32), vertex_insert(AGE_PID, &[10]));
        let err = pollster::block_on(pending::flush_pending(Some(&index)))
            .expect_err("injected failure on the 2nd vertex insert");
        assert!(err.to_string().contains("inv_oracle_injected_failure"));
        assert!(
            index.state.borrow().vertex.is_empty(),
            "compensation must roll the failed batch back to the pre-batch state"
        );
        assert!(
            !graph.repair_journal_is_empty(),
            "failed batch is journaled"
        );

        // Batch 2 — edge property postings: two slots on the same (owner, label).
        edge_pending::push_edge_index_op(
            VertexId::from(OWNER),
            KNOWS_LABEL,
            0,
            edge_insert(&[100]),
        );
        edge_pending::push_edge_index_op(
            VertexId::from(OWNER),
            KNOWS_LABEL,
            1,
            edge_insert(&[200]),
        );
        pollster::block_on(edge_pending::flush_pending(Some(&index))).expect("edge batch flushes");

        // Batch 3 — compaction re-key: slot 0's edge is deleted, then slot 1 is
        // compacted down into slot 0 (remove old slot, insert new slot). This is
        // the only maintenance op that moves slot_index (ADR 0023 established fact).
        edge_pending::push_edge_index_op(
            VertexId::from(OWNER),
            KNOWS_LABEL,
            0,
            edge_remove(&[100]),
        );
        edge_pending::push_edge_index_op(
            VertexId::from(OWNER),
            KNOWS_LABEL,
            1,
            edge_remove(&[200]),
        );
        edge_pending::push_edge_index_op(
            VertexId::from(OWNER),
            KNOWS_LABEL,
            0,
            edge_insert(&[200]),
        );
        pollster::block_on(edge_pending::flush_pending(Some(&index)))
            .expect("compaction re-key flushes");

        // Batch 4 — always-on vertex label membership.
        label_pending::record_vertex_label_set(
            VertexId::from(1u32),
            &[],
            &[VertexLabelId::from_raw(KNOWS_LABEL)],
        );
        label_pending::record_vertex_label_set(
            VertexId::from(2u32),
            &[],
            &[VertexLabelId::from_raw(KNOWS_LABEL)],
        );
        pollster::block_on(label_pending::flush_pending(Some(&index)))
            .expect("label batch flushes");

        // Repair: replay the journaled vertex batch (the index is healthy now).
        pollster::block_on(drain_once(&index)).expect("journal drains clean");
        assert!(graph.repair_journal_is_empty(), "journal fully drained");

        let expected = IndexProjection {
            vertex: BTreeSet::from([
                (AGE_PID, vec![10], 1),
                (AGE_PID, vec![11], 2),
                (AGE_PID, vec![10], 3),
            ]),
            // Only the compacted slot 0 survives; the deleted/old-slot postings are gone.
            edge: BTreeSet::from([(WEIGHT_PID, vec![200], KNOWS_LABEL, OWNER, 0)]),
            label: BTreeSet::from([(u32::from(KNOWS_LABEL), 1), (u32::from(KNOWS_LABEL), 2)]),
        };
        assert_eq!(
            *index.state.borrow(),
            expected,
            "INV: postings must equal the store projection (no missing, no orphan)"
        );

        // Re-application is idempotent: a second drain is a clean no-op.
        pollster::block_on(drain_once(&index)).expect("idempotent re-drain");
        assert_eq!(*index.state.borrow(), expected);
    });
}
