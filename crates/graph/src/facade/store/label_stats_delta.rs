//! Label stats delta log and graph mutation journal (ADR 0015).

use super::GraphStore;
use crate::facade::stable::label_stats_delta::GraphMutationJournalEntry;
use crate::facade::stable::{GRAPH_MUTATION_JOURNAL, LABEL_STATS_DELTA_LOG, LABEL_STATS_DELTA_SEQ};
use gleaph_graph_kernel::federation::LocalVertexId;
use gleaph_graph_kernel::plan_exec::{
    GraphBulkMutationProgress, GraphMutationJournalEntryWire, GraphMutationRequestIdentityV1,
    GraphMutationRetirementV1, GraphOrderedEdgeBatchReceiptV1, GraphOrderedEdgeBatchResult,
    GraphOrderedEdgeBatchResultV1, GraphOrderedMixedBatchReceiptV1, GraphOrderedMixedBatchResult,
    GraphOrderedMixedBatchResultV1, GraphOrderedVertexBatchReceiptV1,
    GraphOrderedVertexBatchResult, GraphOrderedVertexBatchResultV1, LabelStatsDelta,
    LabelStatsDeltaEventWire, MutationId, MutationJournalState, OrderedMixedMutationRetirementAck,
    OrderedMixedMutationRetirementAckV1, OrderedMutationRetirementAck,
    OrderedMutationRetirementAckV1, OrderedVertexMutationRetirementAck,
    OrderedVertexMutationRetirementAckV1, ShardEventSeq,
};
use std::cell::RefCell;

#[cfg(feature = "canbench")]
use canbench_rs::bench_scope as canbench_scope;

/// Journal GC window (ADR 0027), also bounding an individual suspended PlanExecution
/// attempt. This is not a bound on Router resend permission: recoverable Router records
/// can outlive age-only PlanExecution evidence (GAP-2026-09-22-001). The continuation
/// fence prevents an old request from resuming after this window; a *new* request still
/// needs the pending scalar-lifetime/retirement repair.
const GRAPH_MUTATION_JOURNAL_RETENTION_NS: u64 = 9 * 24 * 60 * 60 * 1_000_000_000;

/// Entries examined per amortized GC step on the completed-journal write path
/// (ADR 0025 mechanism B, mirrored). Each completed mutation evicts up to this many
/// expired entries, keeping eviction in pace with the sole growth source (new mutations).
const MUTATION_JOURNAL_GC_BUDGET: usize = 2;

/// Minimum journal length before the amortized retention sweep runs. The sweep has a
/// large fixed cost (stable B-tree range cursor setup), so it is skipped while the journal
/// is short. Once the length reaches this threshold the sweep resumes at the normal
/// amortized rate, keeping long-term growth bounded. The threshold is heap-only state and
/// does not affect the stable layout.
const MUTATION_JOURNAL_GC_MIN_LEN: u64 = 16;

thread_local! {
    /// Ephemeral round-robin cursor for amortized journal GC (ADR 0027). Heap-only on
    /// purpose: resetting to the start on upgrade just restarts the lap, and the journal
    /// itself (region 39, Canonical) is fully stable.
    static MUTATION_JOURNAL_GC_CURSOR: RefCell<Option<MutationId>> = const { RefCell::new(None) };
}

fn ic_time_ns() -> u64 {
    #[cfg(target_family = "wasm")]
    {
        ic_cdk::api::time()
    }
    #[cfg(not(target_family = "wasm"))]
    {
        0
    }
}

#[cfg(test)]
pub(crate) fn reset_mutation_journal_gc_cursor_for_test() {
    MUTATION_JOURNAL_GC_CURSOR.with_borrow_mut(|cursor| *cursor = None);
}

impl GraphStore {
    pub(crate) fn mutation_journal_entry(
        &self,
        mutation_id: MutationId,
    ) -> Option<GraphMutationJournalEntry> {
        GRAPH_MUTATION_JOURNAL.with_borrow(|m| m.get(mutation_id))
    }

    /// Journal-first lookup for a request whose identity is already decoded and bounded.
    ///
    /// `None` is the only result that permits a caller to continue into mutable planning and
    /// catalog validation. An existing entry must carry the exact same request-kind identity;
    /// otherwise the caller must fail before any canonical write.
    pub(crate) fn mutation_journal_entry_for_replay(
        &self,
        mutation_id: MutationId,
        expected_identity: &GraphMutationRequestIdentityV1,
    ) -> Result<Option<GraphMutationJournalEntry>, &'static str> {
        let Some(entry) = self.mutation_journal_entry(mutation_id) else {
            return Ok(None);
        };
        if entry.request_identity() != expected_identity {
            return Err("mutation journal identity conflicts with replay request");
        }
        // Re-validate the durable state at the owner boundary before exposing it to a replay
        // caller. This also rejects an ordered entry that was somehow persisted incompletely.
        entry.wire().validate()?;
        Ok(Some(entry))
    }

    /// Fence an already-admitted PlanExecution future at actual poll re-entry.
    ///
    /// The caller performs its initial journal-first replay check in the first poll. Every
    /// subsequent poll validates the state observed when this future last suspended. Its
    /// own synchronous progress is allowed; another invocation's terminal publication wins
    /// without polling the old continuation. `None` supplies the enclosing transport batch's
    /// time fence, including operations that have not started yet.
    ///
    /// This neither rolls back earlier message segments nor permanently fences a mutation ID.
    pub(crate) async fn guard_plan_continuation<F: std::future::Future>(
        &self,
        mutation_id: Option<MutationId>,
        execution: F,
    ) -> Result<std::ops::ControlFlow<GraphMutationJournalEntry, F::Output>, &'static str> {
        self.guard_plan_continuation_with_clock(mutation_id, execution, ic_time_ns)
            .await
    }

    async fn guard_plan_continuation_with_clock<F: std::future::Future>(
        &self,
        mutation_id: Option<MutationId>,
        execution: F,
        now: impl Fn() -> u64,
    ) -> Result<std::ops::ControlFlow<GraphMutationJournalEntry, F::Output>, &'static str> {
        use std::{future::poll_fn, ops::ControlFlow, pin::pin, task::Poll};
        let started_at_ns = now();
        let mut execution = pin!(execution);
        // Outer None means the initial poll; Some(None) means it suspended without a journal.
        let mut observed = None;
        let journal = || {
            mutation_id.map_or(Ok(None), |id| {
                self.mutation_journal_entry_for_replay(
                    id,
                    &GraphMutationRequestIdentityV1::PlanExecution,
                )
            })
        };
        poll_fn(|cx| {
            if !now()
                .checked_sub(started_at_ns)
                .is_some_and(|elapsed| elapsed <= GRAPH_MUTATION_JOURNAL_RETENTION_NS)
            {
                return Poll::Ready(Err("plan execution attempt expired while suspended"));
            }
            if let Some(previous) = &observed {
                let current = journal()?;
                if &current != previous {
                    return Poll::Ready(match current {
                        Some(entry) if entry.is_completed() => Ok(ControlFlow::Break(entry)),
                        _ => Err("plan execution journal changed while suspended"),
                    });
                }
            }
            match execution.as_mut().poll(cx) {
                Poll::Ready(result) => Poll::Ready(Ok(ControlFlow::Continue(result))),
                Poll::Pending => {
                    // The future may have written in this segment. An impossible family change
                    // is a post-effect invariant failure, not a recoverable partial-write Err.
                    observed = Some(journal().expect("PlanExecution changed its journal family"));
                    Poll::Pending
                }
            }
        })
        .await
    }

    fn ordered_receipt(
        entry: &GraphMutationJournalEntry,
    ) -> Result<GraphOrderedEdgeBatchReceiptV1, &'static str> {
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: entry.row_count(),
            emitted_delta_first_seq: entry.emitted_delta_first_seq(),
            emitted_delta_last_seq: entry.emitted_delta_last_seq(),
            hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub(crate) fn ordered_edge_batch_replay_result(
        &self,
        mutation_id: MutationId,
        expected_identity: &GraphMutationRequestIdentityV1,
    ) -> Result<Option<GraphOrderedEdgeBatchResult>, &'static str> {
        let Some(entry) = self.mutation_journal_entry_for_replay(mutation_id, expected_identity)?
        else {
            return Ok(None);
        };
        let GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            graph_request_fingerprint,
            ..
        } = expected_identity
        else {
            return Err("ordered replay requires OrderedEdgeBatch identity");
        };
        Ok(Some(Self::project_ordered_edge_replay(
            &entry,
            mutation_id,
            graph_request_fingerprint,
        )?))
    }

    /// Resumable ordered-edge replay (ADR 0060): addressed by mutation_id and the stable request
    /// fingerprint. The committed prefix length in the stored identity is a result of execution,
    /// not an input, so it is deliberately excluded from the comparison; the Router pins one child
    /// mutation per committed chunk and its durable receipt pins the exact request.
    pub(crate) fn ordered_edge_batch_resumable_replay_result(
        &self,
        mutation_id: MutationId,
        fingerprint: [u8; 32],
    ) -> Result<Option<GraphOrderedEdgeBatchResult>, &'static str> {
        let Some(entry) = self.mutation_journal_entry(mutation_id) else {
            return Ok(None);
        };
        match entry.request_identity() {
            GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                graph_request_fingerprint,
                ..
            } if *graph_request_fingerprint == fingerprint => {}
            _ => return Err("mutation journal identity conflicts with resumable edge replay"),
        }
        entry.wire().validate()?;
        Ok(Some(Self::project_ordered_edge_replay(
            &entry,
            mutation_id,
            &fingerprint,
        )?))
    }

    fn project_ordered_edge_replay(
        entry: &GraphMutationJournalEntry,
        mutation_id: MutationId,
        fingerprint: &[u8; 32],
    ) -> Result<GraphOrderedEdgeBatchResult, &'static str> {
        let result = match entry.retirement() {
            GraphMutationRetirementV1::Active => {
                GraphOrderedEdgeBatchResultV1::Completed(Self::ordered_receipt(entry)?)
            }
            GraphMutationRetirementV1::Retired { .. } => {
                GraphOrderedEdgeBatchResultV1::MutationRetired {
                    mutation_id,
                    graph_request_fingerprint: *fingerprint,
                }
            }
            GraphMutationRetirementV1::NotApplicable => {
                return Err("ordered replay entry has no retirement state");
            }
        };
        let result = GraphOrderedEdgeBatchResult::V1(result);
        result.validate()?;
        Ok(result)
    }

    fn ordered_vertex_receipt(
        entry: &GraphMutationJournalEntry,
    ) -> Result<GraphOrderedVertexBatchReceiptV1, &'static str> {
        let receipt = GraphOrderedVertexBatchReceiptV1 {
            logical_vertex_count: entry.row_count(),
            emitted_delta_first_seq: entry.emitted_delta_first_seq(),
            emitted_delta_last_seq: entry.emitted_delta_last_seq(),
            hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
            allocated_vertex_ids: entry
                .allocated_vertex_ids()
                .ok_or("ordered vertex journal entry missing allocated vertex ids")?
                .to_vec(),
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub(crate) fn ordered_vertex_batch_replay_result(
        &self,
        mutation_id: MutationId,
        expected_identity: &GraphMutationRequestIdentityV1,
    ) -> Result<Option<GraphOrderedVertexBatchResult>, &'static str> {
        let Some(entry) = self.mutation_journal_entry_for_replay(mutation_id, expected_identity)?
        else {
            return Ok(None);
        };
        let GraphMutationRequestIdentityV1::OrderedVertexBatch {
            graph_request_fingerprint,
            ..
        } = expected_identity
        else {
            return Err("ordered vertex replay requires OrderedVertexBatch identity");
        };
        Ok(Some(Self::project_ordered_vertex_replay(
            &entry,
            mutation_id,
            graph_request_fingerprint,
        )?))
    }

    /// Resumable ordered-vertex replay (ADR 0060): addressed by mutation_id and the stable request
    /// fingerprint; see [`Self::ordered_edge_batch_resumable_replay_result`] for the rationale.
    pub(crate) fn ordered_vertex_batch_resumable_replay_result(
        &self,
        mutation_id: MutationId,
        fingerprint: [u8; 32],
    ) -> Result<Option<GraphOrderedVertexBatchResult>, &'static str> {
        let Some(entry) = self.mutation_journal_entry(mutation_id) else {
            return Ok(None);
        };
        match entry.request_identity() {
            GraphMutationRequestIdentityV1::OrderedVertexBatch {
                graph_request_fingerprint,
                ..
            } if *graph_request_fingerprint == fingerprint => {}
            _ => return Err("mutation journal identity conflicts with resumable vertex replay"),
        }
        entry.wire().validate()?;
        Ok(Some(Self::project_ordered_vertex_replay(
            &entry,
            mutation_id,
            &fingerprint,
        )?))
    }

    fn project_ordered_vertex_replay(
        entry: &GraphMutationJournalEntry,
        mutation_id: MutationId,
        fingerprint: &[u8; 32],
    ) -> Result<GraphOrderedVertexBatchResult, &'static str> {
        let result = match entry.retirement() {
            GraphMutationRetirementV1::Active => {
                GraphOrderedVertexBatchResultV1::Completed(Self::ordered_vertex_receipt(entry)?)
            }
            GraphMutationRetirementV1::Retired { .. } => {
                GraphOrderedVertexBatchResultV1::MutationRetired {
                    mutation_id,
                    graph_request_fingerprint: *fingerprint,
                }
            }
            GraphMutationRetirementV1::NotApplicable => {
                return Err("ordered vertex replay entry has no retirement state");
            }
        };
        let result = GraphOrderedVertexBatchResult::V1(result);
        result.validate()?;
        Ok(result)
    }

    pub(crate) fn ordered_mixed_batch_replay_result(
        &self,
        mutation_id: MutationId,
        expected_identity: &GraphMutationRequestIdentityV1,
    ) -> Result<Option<GraphOrderedMixedBatchResult>, &'static str> {
        let Some(entry) = self.mutation_journal_entry_for_replay(mutation_id, expected_identity)?
        else {
            return Ok(None);
        };
        let GraphMutationRequestIdentityV1::OrderedMixedBatch {
            graph_request_fingerprint,
            logical_operation_count,
            logical_vertex_count,
            logical_edge_count,
            ..
        } = expected_identity
        else {
            return Err("mixed replay requires OrderedMixedBatch identity");
        };
        let result = match entry.retirement() {
            GraphMutationRetirementV1::Active => {
                let receipt = GraphOrderedMixedBatchReceiptV1 {
                    logical_operation_count: u64::from(*logical_operation_count),
                    logical_vertex_count: u64::from(*logical_vertex_count),
                    logical_edge_count: u64::from(*logical_edge_count),
                    emitted_delta_first_seq: entry.emitted_delta_first_seq(),
                    emitted_delta_last_seq: entry.emitted_delta_last_seq(),
                    hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
                    allocated_vertex_ids: entry
                        .allocated_vertex_ids()
                        .ok_or("ordered mixed journal entry missing allocated vertex ids")?
                        .to_vec(),
                };
                receipt.validate()?;
                GraphOrderedMixedBatchResultV1::Completed(receipt)
            }
            GraphMutationRetirementV1::Retired { .. } => {
                GraphOrderedMixedBatchResultV1::MutationRetired {
                    mutation_id,
                    graph_request_fingerprint: *graph_request_fingerprint,
                }
            }
            GraphMutationRetirementV1::NotApplicable => {
                return Err("ordered mixed replay entry has no retirement state");
            }
        };
        let result = GraphOrderedMixedBatchResult::V1(result);
        result.validate()?;
        Ok(Some(result))
    }

    pub(crate) fn commit_record_completed_ordered_mixed_batch_journal(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
        allocated_vertex_ids: Vec<LocalVertexId>,
    ) {
        let GraphMutationRequestIdentityV1::OrderedMixedBatch {
            logical_operation_count,
            logical_vertex_count,
            logical_edge_count,
            ..
        } = &request_identity
        else {
            panic!("mixed journal requires OrderedMixedBatch identity");
        };
        let receipt = GraphOrderedMixedBatchReceiptV1 {
            logical_operation_count: u64::from(*logical_operation_count),
            logical_vertex_count: u64::from(*logical_vertex_count),
            logical_edge_count: u64::from(*logical_edge_count),
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: hot_forward_vertices.clone(),
            allocated_vertex_ids: allocated_vertex_ids.clone(),
        };
        receipt
            .validate()
            .expect("invalid ordered mixed journal receipt");
        assert_eq!(row_count, receipt.logical_operation_count);
        let mut entry = GraphMutationJournalEntry::completed(
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            ic_time_ns(),
        );
        entry.set_request_identity(request_identity);
        entry.set_allocated_vertex_ids(Some(allocated_vertex_ids));
        entry.set_retirement(GraphMutationRetirementV1::Active);
        entry
            .wire()
            .validate()
            .expect("invalid ordered mixed journal entry");
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| m.insert(entry));
    }

    pub(crate) fn commit_record_completed_ordered_vertex_batch_journal_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
        allocated_vertex_ids: Vec<LocalVertexId>,
    ) {
        let receipt = GraphOrderedVertexBatchReceiptV1 {
            logical_vertex_count: row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: hot_forward_vertices.clone(),
            allocated_vertex_ids: allocated_vertex_ids.clone(),
        };
        receipt
            .validate()
            .expect("invalid ordered vertex journal receipt");
        let mut entry = GraphMutationJournalEntry::completed(
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            now_ns,
        );
        entry.set_request_identity(request_identity);
        entry.set_allocated_vertex_ids(Some(allocated_vertex_ids));
        entry.set_retirement(GraphMutationRetirementV1::Active);
        entry
            .wire()
            .validate()
            .expect("invalid ordered vertex journal entry");
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| m.insert(entry));
    }

    pub(crate) fn commit_record_completed_ordered_vertex_batch_journal(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
        allocated_vertex_ids: Vec<LocalVertexId>,
    ) {
        self.commit_record_completed_ordered_vertex_batch_journal_at(
            ic_time_ns(),
            mutation_id,
            request_identity,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            allocated_vertex_ids,
        );
    }

    pub fn get_mutation_journal_entry(
        &self,
        mutation_id: MutationId,
    ) -> Option<GraphMutationJournalEntryWire> {
        self.mutation_journal_entry(mutation_id)
            .map(|entry| entry.wire())
    }

    pub(crate) fn commit_record_incomplete_mutation_journal(
        &self,
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
    ) {
        self.commit_record_incomplete_mutation_journal_at(
            ic_time_ns(),
            mutation_id,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
        );
    }

    pub(crate) fn commit_record_incomplete_mutation_journal_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
    ) {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(GraphMutationJournalEntry::incomplete(
                mutation_id,
                emitted_delta_first_seq,
                emitted_delta_last_seq,
                now_ns,
            ));
        });
    }

    pub(crate) fn commit_record_completed_mutation_journal(
        &self,
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) {
        self.commit_record_completed_mutation_journal_at(
            ic_time_ns(),
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
        );
    }

    pub(crate) fn commit_record_completed_mutation_journal_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) {
        #[cfg(feature = "canbench")]
        let _scope_entry = canbench_scope("journal_entry_build");
        let entry = GraphMutationJournalEntry::completed(
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            now_ns,
        );
        #[cfg(feature = "canbench")]
        drop(_scope_entry);
        #[cfg(feature = "canbench")]
        let _scope_insert = canbench_scope("journal_map_insert");
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(entry);
        });
        #[cfg(feature = "canbench")]
        drop(_scope_insert);
        // Amortized retention sweep: the completed-journal write is the per-mutation
        // growth source, so each one funds one bounded eviction step (ADR 0027). To
        // avoid paying the large fixed sweep cost while the journal is short, the sweep
        // is skipped until the stable journal length reaches a heap-only threshold.
        // Above the threshold the normal amortized rate resumes, so long-term growth
        // remains bounded.
        let journal_len = GRAPH_MUTATION_JOURNAL.with_borrow(|m| m.len());
        if journal_len >= MUTATION_JOURNAL_GC_MIN_LEN {
            #[cfg(feature = "canbench")]
            let _scope_gc = canbench_scope("journal_gc_sweep");
            self.gc_mutation_journal_at(now_ns);
            #[cfg(feature = "canbench")]
            drop(_scope_gc);
        }
    }

    /// Atomically record the completed identity and receipt fields for an ordered edge batch.
    /// Ordered batches never use the PlanExecution continuation state.
    pub(crate) fn commit_record_completed_ordered_edge_batch_journal_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) {
        let receipt = GraphOrderedEdgeBatchReceiptV1 {
            logical_edge_count: row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices: hot_forward_vertices.clone(),
        };
        receipt.validate().expect("invalid ordered journal receipt");
        let mut entry = GraphMutationJournalEntry::completed(
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            now_ns,
        );
        entry.set_request_identity(request_identity);
        entry.set_retirement(GraphMutationRetirementV1::Active);
        entry
            .wire()
            .validate()
            .expect("invalid ordered journal entry");
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(entry);
        });
    }

    pub(crate) fn commit_record_completed_ordered_edge_batch_journal(
        &self,
        mutation_id: MutationId,
        request_identity: GraphMutationRequestIdentityV1,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
    ) {
        self.commit_record_completed_ordered_edge_batch_journal_at(
            ic_time_ns(),
            mutation_id,
            request_identity,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
        );
    }

    pub(crate) fn retire_ordered_mutation_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<Option<OrderedMutationRetirementAck>, &'static str> {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|journal| {
            let Some(mut entry) = journal.get(mutation_id) else {
                return Ok(None);
            };
            let GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                graph_request_fingerprint: stored_fingerprint,
                ..
            } = entry.request_identity()
            else {
                return Err("retirement requires an ordered journal entry");
            };
            if *stored_fingerprint != graph_request_fingerprint {
                return Err("retirement fingerprint conflicts with journal identity");
            }
            let receipt = Self::ordered_receipt(&entry)?;
            match entry.retirement() {
                GraphMutationRetirementV1::Active => {
                    entry.set_retirement(GraphMutationRetirementV1::Retired { at_ns: now_ns });
                    journal.insert(entry);
                }
                GraphMutationRetirementV1::Retired { .. } => {}
                GraphMutationRetirementV1::NotApplicable => {
                    return Err("ordered journal entry has no retirement state");
                }
            }
            let ack = OrderedMutationRetirementAck::V1(OrderedMutationRetirementAckV1 {
                mutation_id,
                graph_request_fingerprint,
                receipt,
            });
            ack.validate()?;
            Ok(Some(ack))
        })
    }

    pub(crate) fn retire_ordered_mutation(
        &self,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<Option<OrderedMutationRetirementAck>, &'static str> {
        self.retire_ordered_mutation_at(ic_time_ns(), mutation_id, graph_request_fingerprint)
    }

    pub(crate) fn retire_ordered_mixed_mutation_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<Option<OrderedMixedMutationRetirementAck>, &'static str> {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|journal| {
            let Some(mut entry) = journal.get(mutation_id) else {
                return Ok(None);
            };
            let GraphMutationRequestIdentityV1::OrderedMixedBatch {
                graph_request_fingerprint: stored_fingerprint,
                logical_operation_count,
                logical_vertex_count,
                logical_edge_count,
                ..
            } = entry.request_identity()
            else {
                return Err("retirement requires an ordered mixed journal entry");
            };
            if *stored_fingerprint != graph_request_fingerprint {
                return Err("retirement fingerprint conflicts with journal identity");
            }
            let receipt = GraphOrderedMixedBatchReceiptV1 {
                logical_operation_count: u64::from(*logical_operation_count),
                logical_vertex_count: u64::from(*logical_vertex_count),
                logical_edge_count: u64::from(*logical_edge_count),
                emitted_delta_first_seq: entry.emitted_delta_first_seq(),
                emitted_delta_last_seq: entry.emitted_delta_last_seq(),
                hot_forward_vertices: entry.hot_forward_vertices().to_vec(),
                allocated_vertex_ids: entry
                    .allocated_vertex_ids()
                    .ok_or("ordered mixed journal entry missing allocated vertex ids")?
                    .to_vec(),
            };
            receipt.validate()?;
            match entry.retirement() {
                GraphMutationRetirementV1::Active => {
                    entry.set_retirement(GraphMutationRetirementV1::Retired { at_ns: now_ns });
                    journal.insert(entry);
                }
                GraphMutationRetirementV1::Retired { .. } => {}
                GraphMutationRetirementV1::NotApplicable => {
                    return Err("ordered mixed journal entry has no retirement state");
                }
            }
            let ack = OrderedMixedMutationRetirementAck::V1(OrderedMixedMutationRetirementAckV1 {
                mutation_id,
                graph_request_fingerprint,
                receipt,
            });
            ack.validate()?;
            Ok(Some(ack))
        })
    }

    pub(crate) fn retire_ordered_mixed_mutation(
        &self,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<Option<OrderedMixedMutationRetirementAck>, &'static str> {
        self.retire_ordered_mixed_mutation_at(ic_time_ns(), mutation_id, graph_request_fingerprint)
    }

    pub(crate) fn retire_ordered_vertex_mutation_at(
        &self,
        now_ns: u64,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<Option<OrderedVertexMutationRetirementAck>, &'static str> {
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|journal| {
            let Some(mut entry) = journal.get(mutation_id) else {
                return Ok(None);
            };
            let GraphMutationRequestIdentityV1::OrderedVertexBatch {
                graph_request_fingerprint: stored_fingerprint,
                ..
            } = entry.request_identity()
            else {
                return Err("vertex retirement requires an ordered vertex journal entry");
            };
            if *stored_fingerprint != graph_request_fingerprint {
                return Err("vertex retirement fingerprint conflicts with journal identity");
            }
            let receipt = Self::ordered_vertex_receipt(&entry)?;
            match entry.retirement() {
                GraphMutationRetirementV1::Active => {
                    entry.set_retirement(GraphMutationRetirementV1::Retired { at_ns: now_ns });
                    journal.insert(entry);
                }
                GraphMutationRetirementV1::Retired { .. } => {}
                GraphMutationRetirementV1::NotApplicable => {
                    return Err("ordered vertex journal entry has no retirement state");
                }
            }
            let ack =
                OrderedVertexMutationRetirementAck::V1(OrderedVertexMutationRetirementAckV1 {
                    mutation_id,
                    graph_request_fingerprint,
                    receipt,
                });
            ack.validate()?;
            Ok(Some(ack))
        })
    }

    pub(crate) fn retire_ordered_vertex_mutation(
        &self,
        mutation_id: MutationId,
        graph_request_fingerprint: [u8; 32],
    ) -> Result<Option<OrderedVertexMutationRetirementAck>, &'static str> {
        self.retire_ordered_vertex_mutation_at(ic_time_ns(), mutation_id, graph_request_fingerprint)
    }

    /// Record an internal plan-batch journal entry with an operation cursor and progress metadata.
    /// `next_index` is the first unexecuted operation; `completed_count` is the number of
    /// operations whose canonical write has already committed.
    pub(crate) fn commit_record_bulk_mutation_journal(
        &self,
        mutation_id: MutationId,
        row_count: u64,
        emitted_delta_first_seq: Option<ShardEventSeq>,
        emitted_delta_last_seq: Option<ShardEventSeq>,
        hot_forward_vertices: Vec<LocalVertexId>,
        operation_count: u32,
        completed_count: u32,
        operation_row_counts: Vec<u64>,
        next_index: Option<u32>,
    ) {
        let now_ns = ic_time_ns();
        let mut entry = GraphMutationJournalEntry::completed(
            mutation_id,
            row_count,
            emitted_delta_first_seq,
            emitted_delta_last_seq,
            hot_forward_vertices,
            now_ns,
        );
        entry.set_next_index(next_index);
        entry.set_bulk_progress(Some(GraphBulkMutationProgress::new(
            operation_count,
            completed_count,
            operation_row_counts,
        )));
        // If the bulk group is not fully done, downgrade the state to Incomplete so retries resume.
        if next_index.is_some() || completed_count != operation_count {
            entry.set_state(MutationJournalState::Incomplete);
        }
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.insert(entry);
        });
    }

    /// One bounded, round-robin retention step over the mutation journal (ADR 0027).
    /// Advances a heap-only cursor; wraps to the start once the keyspace is exhausted.
    pub(crate) fn gc_mutation_journal_at(&self, now_ns: u64) {
        let start = MUTATION_JOURNAL_GC_CURSOR.with_borrow(|cursor| *cursor);
        let (scanned, _removed, last_key) = GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            m.evict_expired(
                start,
                MUTATION_JOURNAL_GC_BUDGET,
                now_ns,
                GRAPH_MUTATION_JOURNAL_RETENTION_NS,
            )
        });
        let next = if (scanned as usize) < MUTATION_JOURNAL_GC_BUDGET {
            None
        } else {
            last_key
        };
        MUTATION_JOURNAL_GC_CURSOR.with_borrow_mut(|cursor| *cursor = next);
    }

    /// Count of entries in the graph mutation journal (PocketIC E2E only).
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn e2e_mutation_journal_len(&self) -> u64 {
        GRAPH_MUTATION_JOURNAL.with_borrow(|m| m.len())
    }

    /// Run a full retention sweep over the whole journal keyspace at the current IC time and return
    /// the remaining entry count (PocketIC E2E only). Drives the production [`evict_expired`] path
    /// with the real 9-day [`GRAPH_MUTATION_JOURNAL_RETENTION_NS`] window — unlike the amortized,
    /// budgeted [`gc_mutation_journal_at`] step it loops until the keyspace is exhausted, so a test
    /// can advance past the window and observe eviction deterministically.
    #[cfg(feature = "pocket-ic-e2e")]
    pub(crate) fn e2e_evict_mutation_journal(&self) -> u64 {
        let now = ic_time_ns();
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|m| {
            let mut cursor = None;
            loop {
                let (scanned, _removed, last) = m.evict_expired(
                    cursor,
                    MUTATION_JOURNAL_GC_BUDGET,
                    now,
                    GRAPH_MUTATION_JOURNAL_RETENTION_NS,
                );
                if (scanned as usize) < MUTATION_JOURNAL_GC_BUDGET {
                    break;
                }
                cursor = last;
            }
            m.len()
        })
    }

    pub(crate) fn commit_append_label_stats_delta(
        &self,
        mutation_id: MutationId,
        label_stats_delta: LabelStatsDelta,
    ) -> Result<LabelStatsDeltaEventWire, String> {
        #[cfg(feature = "canbench")]
        let _scope_seq = canbench_scope("label_stats_delta_seq_alloc");
        let shard_event_seq = LABEL_STATS_DELTA_SEQ.with_borrow_mut(|seq| {
            let current = *seq.get();
            let next = current
                .checked_add(1)
                .ok_or_else(|| "label stats delta sequence exhausted".to_string())?;
            if next == 0 {
                return Err("label stats delta sequence exhausted".to_string());
            }
            seq.set(next);
            Ok::<ShardEventSeq, String>(next)
        })?;
        #[cfg(feature = "canbench")]
        drop(_scope_seq);
        #[cfg(feature = "canbench")]
        let _scope_event = canbench_scope("label_stats_delta_event_build");
        let event = LabelStatsDeltaEventWire {
            mutation_id,
            shard_event_seq,
            label_stats_delta,
        };
        #[cfg(feature = "canbench")]
        drop(_scope_event);
        #[cfg(feature = "canbench")]
        let _scope_log = canbench_scope("label_stats_delta_log_insert");
        LABEL_STATS_DELTA_LOG.with_borrow_mut(|log| {
            log.insert(event.clone());
        });
        #[cfg(feature = "canbench")]
        drop(_scope_log);
        Ok(event)
    }

    pub fn ack_label_stats_deltas_through(&self, through_seq: ShardEventSeq) {
        LABEL_STATS_DELTA_LOG.with_borrow_mut(|log| {
            log.remove_through(through_seq);
        });
    }

    pub fn pending_label_stats_deltas(
        &self,
        from_seq: ShardEventSeq,
        limit: u32,
    ) -> Vec<LabelStatsDeltaEventWire> {
        LABEL_STATS_DELTA_LOG.with_borrow(|log| log.list_from(from_seq, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::VertexLabelId;

    #[test]
    fn persists_lists_and_acks_label_stats_deltas() {
        let store = GraphStore::new();
        let event = store
            .commit_append_label_stats_delta(
                7,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(3), 2)],
                    edge: vec![],
                },
            )
            .expect("persist delta");

        assert_eq!(event.mutation_id, 7);
        assert!(event.shard_event_seq > 0);
        assert_eq!(
            store.pending_label_stats_deltas(event.shard_event_seq, 10),
            vec![event.clone()]
        );

        store.ack_label_stats_deltas_through(event.shard_event_seq);
        assert!(
            !store
                .pending_label_stats_deltas(event.shard_event_seq, 10)
                .iter()
                .any(|pending| pending.shard_event_seq == event.shard_event_seq)
        );
    }

    #[test]
    fn ack_label_stats_deltas_through_removes_prefix() {
        let store = GraphStore::new();
        let first = store
            .commit_append_label_stats_delta(
                1,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(1), 1)],
                    edge: vec![],
                },
            )
            .expect("first delta");
        let second = store
            .commit_append_label_stats_delta(
                1,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(1), 1)],
                    edge: vec![],
                },
            )
            .expect("second delta");

        store.ack_label_stats_deltas_through(first.shard_event_seq);
        assert_eq!(
            store.pending_label_stats_deltas(0, 10),
            vec![second.clone()]
        );
    }

    const DAY_NS: u64 = 24 * 60 * 60 * 1_000_000_000;

    async fn pause_once() {
        let mut first = true;
        std::future::poll_fn(move |cx| {
            if std::mem::take(&mut first) {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(())
            }
        })
        .await;
    }

    #[test]
    fn plan_continuation_replays_positive_and_zero_before_resuming() {
        use ic_stable_structures::Storable;
        use std::{
            cell::Cell,
            future::Future,
            ops::ControlFlow,
            pin::pin,
            task::{Context, Poll, Waker},
        };
        let store = GraphStore::new();
        for count in [0, 1] {
            let id = 91_000 + count;
            let writes = Cell::new(0);
            let mut future = pin!(store.guard_plan_continuation_with_clock(
                Some(id),
                async {
                    pause_once().await;
                    writes.set(writes.get() + 1);
                },
                || 10
            ));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            store.commit_record_completed_mutation_journal_at(
                11,
                id,
                count,
                Some(7),
                Some(8),
                vec![3, 9],
            );
            let saved = store.mutation_journal_entry(id).unwrap();
            let bytes = saved.to_bytes().into_owned();
            assert_eq!(
                future.as_mut().poll(&mut cx),
                Poll::Ready(Ok(ControlFlow::Break(saved)))
            );
            assert_eq!(
                writes.get(),
                0,
                "terminal outcome must win before the inner future is polled"
            );
            assert_eq!(
                store
                    .mutation_journal_entry(id)
                    .unwrap()
                    .to_bytes()
                    .as_ref(),
                bytes
            );
        }
    }

    #[test]
    fn plan_continuation_fences_expired_attempt_after_real_gc() {
        use std::{
            cell::Cell,
            future::Future,
            pin::pin,
            task::{Context, Poll, Waker},
        };
        let store = GraphStore::new();
        // None models a transport batch: its later operations must not start fresh clocks
        // after an earlier operation's call has suspended the enclosing request.
        for mutation_id in [Some(91_100), None] {
            let now = Cell::new(10);
            let writes = Cell::new(0);
            let mut future = pin!(store.guard_plan_continuation_with_clock(
                mutation_id,
                async {
                    pause_once().await;
                    writes.set(writes.get() + 1);
                },
                || now.get()
            ));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            store.commit_record_completed_mutation_journal_at(20, 91_100, 1, None, None, vec![]);
            now.set(20 + GRAPH_MUTATION_JOURNAL_RETENTION_NS + 1);
            reset_mutation_journal_gc_cursor_for_test();
            for _ in 0..256 {
                store.gc_mutation_journal_at(now.get());
                if store.mutation_journal_entry(91_100).is_none() {
                    break;
                }
            }
            assert!(
                store.mutation_journal_entry(91_100).is_none(),
                "real production GC must erase the expired control/outcome"
            );
            assert_eq!(
                future.as_mut().poll(&mut cx),
                Poll::Ready(Err("plan execution attempt expired while suspended"))
            );
            assert_eq!(
                writes.get(),
                0,
                "absence after GC must not revive an old continuation"
            );
        }
    }

    #[test]
    fn plan_continuation_time_boundary_and_own_progress() {
        use std::{
            cell::Cell,
            future::Future,
            ops::ControlFlow,
            pin::pin,
            task::{Context, Poll, Waker},
        };
        let store = GraphStore::new();
        for (start, resume, allowed) in [
            (10, 10 + GRAPH_MUTATION_JOURNAL_RETENTION_NS, true),
            (10, 11 + GRAPH_MUTATION_JOURNAL_RETENTION_NS, false),
            (10, 9, false),
            (u64::MAX - 3, u64::MAX, true),
        ] {
            let now = Cell::new(start);
            let writes = Cell::new(0);
            let mut future = pin!(store.guard_plan_continuation_with_clock(
                None,
                async {
                    pause_once().await;
                    writes.set(writes.get() + 1);
                    23
                },
                || now.get()
            ));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            now.set(resume);
            let expected = if allowed {
                Ok(ControlFlow::Continue(23))
            } else {
                Err("plan execution attempt expired while suspended")
            };
            assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(expected));
            assert_eq!(writes.get(), u32::from(allowed));
        }

        let mut future = pin!(store.guard_plan_continuation_with_clock(
            Some(91_200),
            async {
                store.commit_record_incomplete_mutation_journal_at(1, 91_200, Some(1), Some(1));
                pause_once().await;
                store.commit_record_incomplete_mutation_journal_at(2, 91_200, Some(1), Some(2));
                pause_once().await;
                store.commit_record_completed_mutation_journal_at(
                    3,
                    91_200,
                    2,
                    Some(1),
                    Some(2),
                    vec![],
                );
                pause_once().await; // e.g. the batch's post-journal outbox drain
                2
            },
            || 10
        ));
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..3 {
            assert!(future.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Ok(ControlFlow::Continue(2)))
        );
        assert_eq!(
            store
                .mutation_journal_entry(91_200)
                .unwrap()
                .recorded_at_ns(),
            Some(3)
        );
    }

    #[test]
    fn plan_continuation_rejects_changed_incomplete_and_wrong_family() {
        use std::{
            cell::Cell,
            future::Future,
            pin::pin,
            task::{Context, Poll, Waker},
        };
        let store = GraphStore::new();
        for ordered in [false, true] {
            let id = 91_300 + u64::from(ordered);
            let writes = Cell::new(0);
            let mut future = pin!(store.guard_plan_continuation_with_clock(
                Some(id),
                async {
                    pause_once().await;
                    writes.set(writes.get() + 1);
                },
                || 10
            ));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut cx).is_pending());
            let error = if ordered {
                store.commit_record_completed_ordered_edge_batch_journal_at(
                    11,
                    id,
                    GraphMutationRequestIdentityV1::OrderedEdgeBatch {
                        canonical_encoding_version: 1,
                        graph_request_fingerprint: [8; 32],
                        logical_item_count: 1,
                    },
                    1,
                    None,
                    None,
                    vec![],
                );
                "mutation journal identity conflicts with replay request"
            } else {
                store.commit_record_incomplete_mutation_journal_at(11, id, None, None);
                "plan execution journal changed while suspended"
            };
            let saved = store.mutation_journal_entry(id);
            assert_eq!(future.as_mut().poll(&mut cx), Poll::Ready(Err(error)));
            assert_eq!(writes.get(), 0);
            assert_eq!(store.mutation_journal_entry(id), saved);
        }
    }

    #[test]
    fn completed_write_path_stamps_and_amortized_gc_evicts() {
        super::reset_mutation_journal_gc_cursor_for_test();
        let store = GraphStore::new();
        let id = 9_000_001u64;
        // Recorded "long ago" via the timestamped write path.
        store.commit_record_completed_mutation_journal_at(0, id, 3, None, None, Vec::new());
        let entry = store.mutation_journal_entry(id).expect("entry recorded");
        assert_eq!(entry.recorded_at_ns(), Some(0));

        // Drive amortized GC forward at a time well past retention; the round-robin cursor
        // reaches the aged entry within a bounded number of budgeted steps and evicts it.
        let now = 100 * DAY_NS;
        for _ in 0..256 {
            if store.mutation_journal_entry(id).is_none() {
                break;
            }
            store.gc_mutation_journal_at(now);
        }
        assert!(
            store.mutation_journal_entry(id).is_none(),
            "aged entry should be evicted by amortized GC"
        );
    }

    #[test]
    fn mutation_journal_roundtrips_seq_range() {
        let store = GraphStore::new();
        let event = store
            .commit_append_label_stats_delta(
                11,
                LabelStatsDelta {
                    vertex: vec![(VertexLabelId::from_raw(4), 1)],
                    edge: vec![],
                },
            )
            .expect("persist delta");
        store.commit_record_completed_mutation_journal(
            11,
            5,
            Some(event.shard_event_seq),
            Some(event.shard_event_seq),
            vec![7, 42],
        );

        let journal = store.mutation_journal_entry(11).expect("journal entry");
        assert!(journal.is_completed());
        assert_eq!(journal.row_count(), 5);
        assert_eq!(
            journal.emitted_delta_first_seq(),
            Some(event.shard_event_seq)
        );
        assert_eq!(
            journal.emitted_delta_last_seq(),
            Some(event.shard_event_seq)
        );
        assert_eq!(journal.hot_forward_vertices().to_vec(), vec![7, 42]);

        let wire = store.get_mutation_journal_entry(11).expect("journal wire");
        assert_eq!(wire.emitted_delta_first_seq(), Some(event.shard_event_seq));
        assert_eq!(wire.emitted_delta_last_seq(), Some(event.shard_event_seq));
        assert_eq!(wire.hot_forward_vertices().to_vec(), vec![7, 42]);
    }

    /// An expired journal entry makes its mutation ID look "never attempted", which permits re-execution.
    ///
    /// `mutation_journal_entry_for_replay` documents `None` as the only answer that lets a caller continue into mutable
    /// planning and catalog validation, and `evict_expired` retires `NotApplicable` entries by age alone. Together they
    /// mean that once retention passes, an identical re-dispatch is indistinguishable from a first attempt: the caller
    /// re-plans and re-writes, so a later property value can be overwritten and an earlier zero can become a write.
    ///
    /// `#[ignore]`d because it asserts the **defect**, not the contract: GAP-2026-09-22-001 requires the last assertion
    /// to be a refusal (an expired identity must be permanently inadmissible), and that needs the Graph-side fence the
    /// ledger describes - Active/Retired scalar evidence and exact durable receipts - rather than a larger constant.
    #[test]
    #[ignore = "GAP-2026-09-22-001: reproduces re-execution permitted after retention"]
    fn an_expired_journal_entry_permits_re_execution() {
        let store = GraphStore::new();
        let identity = GraphMutationRequestIdentityV1::PlanExecution;
        let mutation_id = 4_242;
        let recorded_at = 10;
        store.commit_record_completed_mutation_journal_at(
            recorded_at,
            mutation_id,
            0,
            Some(1),
            Some(1),
            Vec::new(),
        );
        // The plan-execution family retires as `NotApplicable`, which is evictable by age alone.
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|journal| {
            let mut entry = journal.get(mutation_id).expect("journaled at admission");
            entry.set_retirement(GraphMutationRetirementV1::NotApplicable);
            journal.insert(entry);
        });
        assert!(store.mutation_journal_entry(mutation_id).is_some());

        // One sweep past the nine-day retention removes it.
        let retention = 9 * 24 * 60 * 60 * 1_000_000_000u64;
        let now = recorded_at + retention + 1;
        GRAPH_MUTATION_JOURNAL.with_borrow_mut(|journal| {
            journal.evict_expired(None, 64, now, retention);
        });
        assert!(
            store.mutation_journal_entry(mutation_id).is_none(),
            "retention evicted the only evidence"
        );

        // The defect: the replay entry now says "nothing was attempted here".
        assert_eq!(
            store.mutation_journal_entry_for_replay(mutation_id, &identity),
            Ok(None),
            "expired evidence is treated as a first attempt, so the caller may re-execute"
        );
    }

    #[test]
    fn ordered_journal_replay_requires_exact_identity_before_writes() {
        let store = GraphStore::new();
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [8; 32],
            logical_item_count: 2,
        };
        store.commit_record_completed_ordered_edge_batch_journal_at(
            12,
            12_000,
            identity.clone(),
            2,
            Some(4),
            Some(5),
            vec![3, 9],
        );

        let replay = store
            .mutation_journal_entry_for_replay(12_000, &identity)
            .expect("matching identity should be accepted")
            .expect("journal entry should exist");
        assert_eq!(replay.row_count(), 2);
        assert_eq!(replay.retirement(), &GraphMutationRetirementV1::Active);

        let conflicting = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [9; 32],
            logical_item_count: 2,
        };
        assert_eq!(
            store.mutation_journal_entry_for_replay(12_000, &conflicting),
            Err("mutation journal identity conflicts with replay request")
        );
        assert!(store.mutation_journal_entry(12_000).is_some());
    }

    #[test]
    fn ordered_journal_replay_allows_absence_for_fresh_execution() {
        let store = GraphStore::new();
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [10; 32],
            logical_item_count: 1,
        };
        assert_eq!(
            store.mutation_journal_entry_for_replay(12_001, &identity),
            Ok(None)
        );
    }

    #[test]
    fn ordered_replay_projects_receipt_and_retirement_is_idempotent() {
        let store = GraphStore::new();
        let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
            canonical_encoding_version: 1,
            graph_request_fingerprint: [11; 32],
            logical_item_count: 2,
        };
        store.commit_record_completed_ordered_edge_batch_journal_at(
            12,
            12_002,
            identity.clone(),
            2,
            Some(6),
            Some(7),
            vec![4, 8],
        );

        let active = store
            .ordered_edge_batch_replay_result(12_002, &identity)
            .expect("active replay lookup")
            .expect("active journal entry");
        assert!(matches!(
            active,
            GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::Completed(_))
        ));

        let ack = store
            .retire_ordered_mutation_at(20, 12_002, [11; 32])
            .expect("retire ordered mutation")
            .expect("retirement ack");
        let repeated = store
            .retire_ordered_mutation_at(21, 12_002, [11; 32])
            .expect("repeat retirement")
            .expect("repeat ack");
        assert_eq!(ack, repeated);

        let retired = store
            .ordered_edge_batch_replay_result(12_002, &identity)
            .expect("retired replay lookup")
            .expect("retired journal entry");
        match retired {
            GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1::MutationRetired {
                mutation_id,
                graph_request_fingerprint,
            }) => {
                assert_eq!(mutation_id, 12_002);
                assert_eq!(graph_request_fingerprint, [11; 32]);
            }
            other => panic!("unexpected retired replay result: {other:?}"),
        }
        assert_eq!(
            store.retire_ordered_mutation_at(22, 12_002, [12; 32]),
            Err("retirement fingerprint conflicts with journal identity")
        );
        assert!(store.mutation_journal_entry(12_002).is_some());
    }
}
