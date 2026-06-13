//! Cursor-based backfill of edge property index postings from canonical `EDGE_PROPERTIES`.

use crate::facade::GraphStore;
use crate::index::lookup::PropertyIndexLookup;
use crate::property::sortable_index_key;
use gleaph_graph_kernel::federation::{EdgePostingBackfillArgs, EdgePostingBackfillResult};

pub async fn backfill_edge_property_postings(
    store: &GraphStore,
    index: &dyn PropertyIndexLookup,
    args: EdgePostingBackfillArgs,
) -> Result<EdgePostingBackfillResult, String> {
    if !store.federation_configured() {
        return Err("federation not configured".into());
    }
    if args.max_entries == 0 {
        return Err("max_entries must be greater than zero".into());
    }
    let shard_id = index.local_shard_id();
    let batch = store.scan_edge_properties_batch(args.after_key.clone(), args.max_entries)?;
    let entries_processed = u32::try_from(batch.len()).unwrap_or(u32::MAX);
    let done = entries_processed < args.max_entries;
    let next_after_key = batch
        .last()
        .map(|(key, _)| GraphStore::edge_property_cursor(*key));
    let mut postings_synced = 0u32;

    for (key, value) in batch {
        if !crate::index::registry::should_maintain_edge_posting(key.label_id(), key.property_id())
        {
            continue;
        }
        let Some(payload_bytes) = sortable_index_key(&value) else {
            continue;
        };
        let owner_raw = u32::from_le_bytes(key.owner_vertex_id().to_le_bytes());
        index
            .edge_posting_insert_at(
                shard_id,
                key.property_id().raw(),
                payload_bytes,
                key.label_id(),
                owner_raw,
                key.slot_index(),
            )
            .await
            .map_err(|e| e.to_string())?;
        postings_synced = postings_synced.saturating_add(1);
    }

    Ok(EdgePostingBackfillResult {
        next_after_key,
        entries_processed,
        postings_synced,
        done,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::PropertyId;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit, PostingRangeRequest};
    use std::sync::Mutex;

    struct RecordingEdgeIndex {
        inserts: Mutex<Vec<(u32, u32, Vec<u8>, u16, u32, u32)>>,
    }

    impl RecordingEdgeIndex {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for RecordingEdgeIndex {
        async fn lookup_equal(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
            _property_id: u32,
            _req: &PostingRangeRequest,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_intersection(
            &self,
            _req: &IndexIntersectionRequest,
        ) -> Result<gleaph_graph_kernel::index::IndexIntersectionResult, crate::plan::PlanQueryError>
        {
            Ok(gleaph_graph_kernel::index::IndexIntersectionResult::Vertices(vec![]))
        }

        fn local_shard_id(&self) -> ShardId {
            ShardId::new(0)
        }

        async fn posting_insert_at(
            &self,
            _shard_id: ShardId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            _shard_id: ShardId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn label_posting_insert_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn label_posting_remove_at(
            &self,
            _shard_id: ShardId,
            _label_id: u32,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn edge_posting_insert_at(
            &self,
            shard_id: ShardId,
            property_id: u32,
            value: Vec<u8>,
            label_id: u16,
            owner_vertex_id: u32,
            slot_index: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            self.inserts.lock().unwrap().push((
                shard_id.raw(),
                property_id,
                value,
                label_id,
                owner_vertex_id,
                slot_index,
            ));
            Ok(())
        }
    }

    fn federated_store() -> GraphStore {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
            }))
            .expect("routing");
        store
    }

    #[test]
    fn backfill_replays_registered_indexable_edge_properties() {
        let store = federated_store();
        let index = RecordingEdgeIndex::new();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let handle = store
            .insert_directed_edge(owner, neighbor, None)
            .expect("edge");
        let canonical = store.canonical_edge_handle(handle);
        let weight = PropertyId::from_raw(55);
        crate::index::registry::register_edge_property(weight);
        store
            .set_edge_property(canonical, weight, Value::Int64(9))
            .expect("weight");

        let result = pollster::block_on(backfill_edge_property_postings(
            &store,
            &index,
            EdgePostingBackfillArgs {
                after_key: None,
                max_entries: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done);
        assert_eq!(result.postings_synced, 1);
        let inserts = index.inserts.lock().unwrap();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].1, 55);
    }
}
