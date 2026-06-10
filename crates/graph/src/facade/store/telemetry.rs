//! Label telemetry domain: shard event outbox and mutation idempotency records.

use super::GraphStore;
use crate::facade::stable::{
    APPLIED_MUTATION_REQUESTS, LABEL_TELEMETRY_OUTBOX, LABEL_TELEMETRY_SEQ,
};
use gleaph_graph_kernel::plan_exec::{
    LabelTelemetryEventWire, LabelUsageDelta, MutationId, MutationOutcomeWire, ShardEventSeq,
};

impl GraphStore {
    pub(crate) fn applied_mutation_request(
        &self,
        mutation_id: MutationId,
    ) -> Option<crate::facade::stable::label_telemetry::AppliedMutationRequest> {
        APPLIED_MUTATION_REQUESTS.with_borrow(|m| m.get(mutation_id))
    }

    pub fn mutation_outcome(&self, mutation_id: MutationId) -> Option<MutationOutcomeWire> {
        self.applied_mutation_request(mutation_id).map(Into::into)
    }

    pub(crate) fn commit_record_incomplete_mutation_request(
        &self,
        mutation_id: MutationId,
        events: Vec<LabelTelemetryEventWire>,
    ) {
        APPLIED_MUTATION_REQUESTS.with_borrow_mut(|m| {
            m.insert(
                crate::facade::stable::label_telemetry::AppliedMutationRequest::incomplete(
                    mutation_id,
                    events,
                ),
            );
        });
    }

    pub(crate) fn commit_record_completed_mutation_request(
        &self,
        mutation_id: MutationId,
        row_count: u64,
        events: Vec<LabelTelemetryEventWire>,
    ) {
        APPLIED_MUTATION_REQUESTS.with_borrow_mut(|m| {
            m.insert(
                crate::facade::stable::label_telemetry::AppliedMutationRequest::completed(
                    mutation_id,
                    row_count,
                    events,
                ),
            );
        });
    }

    pub(crate) fn commit_persist_label_telemetry_event(
        &self,
        mutation_id: MutationId,
        label_usage_delta: LabelUsageDelta,
    ) -> Result<LabelTelemetryEventWire, String> {
        let shard_event_seq = LABEL_TELEMETRY_SEQ.with_borrow_mut(|seq| {
            let current = *seq.get();
            let next = current
                .checked_add(1)
                .ok_or_else(|| "label telemetry event sequence exhausted".to_string())?;
            if next == 0 {
                return Err("label telemetry event sequence exhausted".to_string());
            }
            seq.set(next);
            Ok::<ShardEventSeq, String>(next)
        })?;
        let event = LabelTelemetryEventWire {
            mutation_id,
            shard_event_seq,
            label_usage_delta,
        };
        LABEL_TELEMETRY_OUTBOX.with_borrow_mut(|outbox| {
            outbox.insert(event.clone());
        });
        Ok(event)
    }

    pub fn ack_label_telemetry_event(&self, seq: ShardEventSeq) {
        LABEL_TELEMETRY_OUTBOX.with_borrow_mut(|outbox| {
            outbox.remove(seq);
        });
    }

    pub fn pending_label_telemetry_events(
        &self,
        from_seq: ShardEventSeq,
        limit: u32,
    ) -> Vec<LabelTelemetryEventWire> {
        LABEL_TELEMETRY_OUTBOX.with_borrow(|outbox| outbox.list_from(from_seq, limit))
    }

    /// Compatibility wrappers for existing call sites.
    pub(crate) fn record_incomplete_mutation_request(
        &self,
        mutation_id: MutationId,
        events: Vec<LabelTelemetryEventWire>,
    ) {
        self.commit_record_incomplete_mutation_request(mutation_id, events);
    }

    pub(crate) fn record_completed_mutation_request(
        &self,
        mutation_id: MutationId,
        row_count: u64,
        events: Vec<LabelTelemetryEventWire>,
    ) {
        self.commit_record_completed_mutation_request(mutation_id, row_count, events);
    }

    pub(crate) fn persist_label_telemetry_event(
        &self,
        mutation_id: MutationId,
        label_usage_delta: LabelUsageDelta,
    ) -> Result<LabelTelemetryEventWire, String> {
        self.commit_persist_label_telemetry_event(mutation_id, label_usage_delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::entry::VertexLabelId;

    #[test]
    fn persists_lists_and_acks_label_telemetry_events() {
        let store = GraphStore::new();
        let event = store
            .commit_persist_label_telemetry_event(
                7,
                LabelUsageDelta {
                    vertex: vec![(VertexLabelId::from_raw(3), 2)],
                    edge: vec![],
                },
            )
            .expect("persist event");

        assert_eq!(event.mutation_id, 7);
        assert!(event.shard_event_seq > 0);
        assert_eq!(
            store.pending_label_telemetry_events(event.shard_event_seq, 10),
            vec![event.clone()]
        );

        store.ack_label_telemetry_event(event.shard_event_seq);
        assert!(
            !store
                .pending_label_telemetry_events(event.shard_event_seq, 10)
                .iter()
                .any(|pending| pending.shard_event_seq == event.shard_event_seq)
        );
    }

    #[test]
    fn applied_mutation_request_roundtrips_cached_result() {
        let store = GraphStore::new();
        let event = store
            .commit_persist_label_telemetry_event(
                11,
                LabelUsageDelta {
                    vertex: vec![(VertexLabelId::from_raw(4), 1)],
                    edge: vec![],
                },
            )
            .expect("persist event");
        store.commit_record_completed_mutation_request(11, 5, vec![event.clone()]);

        let cached = store.applied_mutation_request(11).expect("cached request");
        assert!(cached.completed);
        assert_eq!(cached.row_count, 5);
        assert_eq!(cached.label_telemetry_events, vec![event]);

        let outcome = store.mutation_outcome(11).expect("mutation outcome");
        assert!(outcome.completed);
        assert_eq!(outcome.row_count, 5);
        assert_eq!(
            outcome.label_telemetry_events,
            cached.label_telemetry_events
        );
    }
}
