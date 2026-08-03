//! Cursor-based backfill of edge property index postings from canonical `EDGE_PROPERTIES`.

use crate::facade::GraphStore;
use crate::index::lookup::{PropertyIndexLookup, dispatch_posting_batch};
use crate::property::sortable_index_key;
use gleaph_graph_kernel::federation::{EdgePostingBackfillArgs, EdgePostingBackfillResult};
use gleaph_graph_kernel::index::IndexPostingMutation;

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
    let mut index_batch = Vec::new();

    for (key, value) in batch {
        let physical_index_ids = crate::index::catalog_context::active_edge_physical_index_ids(
            key.label_id(),
            key.property_id(),
        );
        if physical_index_ids.is_empty() {
            continue;
        }
        let Some(payload_bytes) = sortable_index_key(&value) else {
            continue;
        };
        let owner_raw = u32::from_le_bytes(key.owner_vertex_id().to_le_bytes());
        for physical_index_id in physical_index_ids {
            if index.supports_posting_batch() {
                index_batch.push(IndexPostingMutation::EdgeProperty {
                    physical_index_id,
                    remove: false,
                    property_id: key.property_id().raw(),
                    value: payload_bytes.clone(),
                    label_id: key.label_id(),
                    owner_vertex_id: owner_raw,
                    slot_index: key.slot_index(),
                });
            } else {
                index
                    .edge_posting_insert_at(
                        shard_id,
                        physical_index_id,
                        key.property_id().raw(),
                        payload_bytes.clone(),
                        key.label_id(),
                        owner_raw,
                        key.slot_index(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
            }
            postings_synced = postings_synced.saturating_add(1);
        }
    }

    if !index_batch.is_empty() {
        dispatch_posting_batch(index, shard_id, index_batch)
            .await
            .map_err(|e| e.to_string())?;
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
    use gleaph_graph_kernel::index::{
        IndexIntersectionRequest, IndexPostingBatchProgress, IndexPostingMutation, PhysicalIndexId,
        PostingHit, PostingRangeRequest,
    };
    use std::sync::Mutex;

    struct RecordingEdgeIndex {
        inserts: Mutex<Vec<(u32, u32, Vec<u8>, u16, u32, u32)>>,
        batches: Mutex<Vec<Vec<IndexPostingMutation>>>,
        batch_mode: bool,
    }

    impl RecordingEdgeIndex {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: false,
            }
        }

        fn batch() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for RecordingEdgeIndex {
        fn supports_posting_batch(&self) -> bool {
            self.batch_mode
        }

        async fn posting_batch_at(
            &self,
            _shard_id: ShardId,
            operations: Vec<IndexPostingMutation>,
        ) -> Result<IndexPostingBatchProgress, crate::plan::PlanQueryError> {
            let applied = operations.len() as u32;
            self.batches.lock().unwrap().push(operations);
            Ok(IndexPostingBatchProgress {
                applied,
                next_index: None,
                instruction_budget_exhausted: false,
            })
        }

        async fn lookup_equal(
            &self,
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, crate::plan::PlanQueryError> {
            Ok(vec![])
        }

        async fn lookup_range(
            &self,
            _physical_index_id: PhysicalIndexId,
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
            _physical_index_id: PhysicalIndexId,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            Ok(())
        }

        async fn posting_remove_at(
            &self,
            _shard_id: ShardId,
            _physical_index_id: PhysicalIndexId,
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
            _physical_index_id: PhysicalIndexId,
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
                vector_index_canister: None,
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
        // Unlabeled wire labels do not map to a catalog label and are intentionally excluded from
        // index membership resolution, so the fixture must use a dedicated labeled edge that the
        // membership catalog registers for the backfill under test.
        let label = crate::test_labels::edge_label_id_for_name("backfill_edge_label");
        let handle = store
            .insert_directed_edge(owner, neighbor, Some(label))
            .expect("edge");
        let weight = PropertyId::from_raw(55);
        let _catalog =
            crate::index::catalog_context::enter_edge_indexed_with_label(&[weight], label);
        store
            .set_edge_property(
                handle.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                weight,
                Value::Int64(9),
            )
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

    #[test]
    fn backfill_batches_multiple_edge_properties() {
        let store = federated_store();
        let index = RecordingEdgeIndex::batch();
        let owner = store.insert_vertex().expect("owner");
        let neighbor = store.insert_vertex().expect("neighbor");
        let label = crate::test_labels::edge_label_id_for_name("backfill_edge_label");
        let handle = store
            .insert_directed_edge(owner, neighbor, Some(label))
            .expect("edge");
        let weight = PropertyId::from_raw(55);
        let distance = PropertyId::from_raw(56);
        let _catalog = crate::index::catalog_context::enter_edge_indexed_with_label(
            &[weight, distance],
            label,
        );
        store
            .set_edge_property(
                handle.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                weight,
                Value::Int64(9),
            )
            .expect("weight");
        store
            .set_edge_property(
                handle.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                distance,
                Value::Int64(12),
            )
            .expect("distance");

        let result = pollster::block_on(backfill_edge_property_postings(
            &store,
            &index,
            EdgePostingBackfillArgs {
                after_key: None,
                max_entries: 10,
            },
        ))
        .expect("backfill");

        assert_eq!(result.postings_synced, 2);
        let batches = index.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }
}
