//! Cursor-based backfill of vertex property index postings from shard-local vertex properties.

use crate::facade::GraphStore;
use crate::index::lookup::{PropertyIndexLookup, dispatch_posting_batch};
use crate::property::sortable_index_key;
use gleaph_graph_kernel::federation::{PostingBackfillArgs, PostingBackfillResult};
use gleaph_graph_kernel::index::IndexPostingMutation;
use ic_stable_lara::VertexId;

pub async fn backfill_vertex_property_postings(
    store: &GraphStore,
    index: &dyn PropertyIndexLookup,
    args: PostingBackfillArgs,
) -> Result<PostingBackfillResult, String> {
    if !store.federation_configured() {
        return Err("federation not configured".into());
    }
    let shard_id = index.local_shard_id();
    let vertex_cap = u32::from(store.vertex_count());
    let max_vertices = args.max_vertices.max(1);
    let mut cursor = args.start_vertex_id.min(vertex_cap);
    let mut vertices_processed = 0u32;
    let mut postings_synced = 0u32;
    let mut batch = Vec::new();

    while vertices_processed < max_vertices && cursor < vertex_cap {
        let vertex_id = VertexId::from(cursor);
        cursor = cursor.saturating_add(1);
        vertices_processed = vertices_processed.saturating_add(1);

        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone() {
            continue;
        }
        let labels = store.vertex_labels(vertex_id, vertex);
        let local_raw = u32::from_le_bytes(vertex_id.to_le_bytes());
        for (property_id, value) in store.vertex_properties(vertex_id) {
            // One resolution owner with DML: flat targets post their own property; nested
            // record targets walk the stored record along the declared leaf path.
            for target in
                crate::index::catalog_context::vertex_index_targets_for_labels(&labels, property_id)
            {
                if !target.membership.phase.is_active() {
                    continue;
                }
                let leaf_value = if target.field_tail.is_empty() {
                    Some(&value)
                } else {
                    crate::property::record_value_at_dotted_path(&value, &target.field_tail)
                        .and_then(crate::property::nested_leaf_posting_value)
                };
                let Some(leaf_value) = leaf_value else {
                    continue;
                };
                let Some(payload_bytes) = sortable_index_key(leaf_value) else {
                    continue;
                };
                let physical_index_id = target.membership.physical_index_id;
                let posting_property_id = target.posting_property_id.raw();
                if index.supports_posting_batch() {
                    batch.push(IndexPostingMutation::VertexProperty {
                        physical_index_id,
                        remove: false,
                        property_id: posting_property_id,
                        value: payload_bytes.clone(),
                        vertex_id: local_raw,
                    });
                } else {
                    index
                        .posting_insert_at(
                            shard_id,
                            physical_index_id,
                            posting_property_id,
                            payload_bytes.clone(),
                            local_raw,
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                }
                postings_synced = postings_synced.saturating_add(1);
            }
        }
    }

    if !batch.is_empty() {
        dispatch_posting_batch(index, shard_id, batch)
            .await
            .map_err(|e| e.to_string())?;
    }

    Ok(PostingBackfillResult {
        next_vertex_id: cursor,
        vertices_processed,
        postings_synced,
        done: cursor >= vertex_cap,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{
        IndexIntersectionRequest, IndexMaintenancePhase, IndexPostingBatchProgress,
        IndexPostingMutation, IndexedPropertyCatalog, IndexedVertexMembership, PhysicalIndexId,
        PostingHit, PostingRangeRequest,
    };
    use std::sync::Mutex;

    struct RecordingIndex {
        inserts: Mutex<Vec<(u32, PhysicalIndexId, u32, Vec<u8>, u32)>>,
        batches: Mutex<Vec<Vec<IndexPostingMutation>>>,
        batch_mode: bool,
        batch_limit: Option<usize>,
        fail_batch: bool,
    }

    impl RecordingIndex {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: false,
                batch_limit: None,
                fail_batch: false,
            }
        }

        fn batch() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
                batch_limit: None,
                fail_batch: false,
            }
        }

        fn batch_with_limit(limit: usize) -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
                batch_limit: Some(limit),
                fail_batch: false,
            }
        }

        fn batch_failure() -> Self {
            Self {
                inserts: Mutex::new(Vec::new()),
                batches: Mutex::new(Vec::new()),
                batch_mode: true,
                batch_limit: None,
                fail_batch: true,
            }
        }
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for RecordingIndex {
        fn supports_posting_batch(&self) -> bool {
            self.batch_mode
        }

        async fn posting_batch_at(
            &self,
            _shard_id: ShardId,
            operations: Vec<IndexPostingMutation>,
        ) -> Result<IndexPostingBatchProgress, crate::plan::PlanQueryError> {
            if self.fail_batch {
                return Err(crate::plan::PlanQueryError::UnsupportedOp(
                    "forced backfill batch failure",
                ));
            }
            let applied = self
                .batch_limit
                .map_or(operations.len(), |limit| limit.min(operations.len()));
            self.batches
                .lock()
                .unwrap()
                .push(operations[..applied].to_vec());
            Ok(IndexPostingBatchProgress {
                applied: applied as u32,
                next_index: (applied < operations.len()).then_some(applied as u32),
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
            shard_id: ShardId,
            physical_index_id: PhysicalIndexId,
            property_id: u32,
            value: Vec<u8>,
            vertex_id: u32,
        ) -> Result<(), crate::plan::PlanQueryError> {
            self.inserts.lock().unwrap().push((
                shard_id.raw(),
                physical_index_id,
                property_id,
                value,
                vertex_id,
            ));
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
    }

    fn federated_store() -> GraphStore {
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: None,
            }))
            .expect("routing");
        store
    }

    #[test]
    fn backfill_replays_indexable_vertex_properties() {
        let store = federated_store();
        let index = RecordingIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let label = VertexLabelId::from_raw(1);
        let vertex = store.vertex(vid).expect("vertex row");
        store
            .set_vertex_labels(vid, vertex, [label])
            .expect("label");
        crate::index::label_pending::clear_pending();
        let name = crate::test_labels::property_id_for_name("backfill_name");
        let score = crate::test_labels::property_id_for_name("backfill_score");
        let target_physical = PhysicalIndexId::new(1_001).expect("target physical id");
        let decoy_physical = PhysicalIndexId::new(1_002).expect("decoy physical id");
        store
            .set_vertex_property(vid, name, Value::Int64(42))
            .expect("name");
        store
            .set_vertex_property(vid, score, Value::Int64(99))
            .expect("score");
        crate::index::pending::clear_pending();
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                IndexedVertexMembership {
                    physical_index_id: target_physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Active,
                    property_id: name.raw(),
                    label_id: 1,
                    field_path: String::new(),
                    ancestor_property_id: 0,
                },
                IndexedVertexMembership {
                    physical_index_id: decoy_physical,
                    catalog_epoch: 1,
                    phase: IndexMaintenancePhase::Active,
                    property_id: name.raw(),
                    label_id: 2,
                    field_path: String::new(),
                    ancestor_property_id: 0,
                },
            ],
            ..Default::default()
        });

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done);
        let inserts = index.inserts.lock().unwrap().clone();
        assert_eq!(
            inserts.len(),
            1,
            "only registered properties are backfilled"
        );
        assert_eq!(inserts[0].0, 0);
        assert_eq!(inserts[0].1, target_physical);
        assert_eq!(inserts[0].2, name.raw());
        assert_eq!(inserts[0].4, u32::from(vid));
        assert!(
            inserts
                .iter()
                .all(|(_, physical_index_id, _, _, _)| *physical_index_id != decoy_physical)
        );
    }

    #[test]
    fn backfill_skips_unindexable_values() {
        let store = federated_store();
        let index = RecordingIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let pid = PropertyId::from_raw(5);
        store
            .set_vertex_property(vid, pid, Value::Float64(f64::NAN))
            .expect("nan property");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert_eq!(result.postings_synced, 0);
        assert!(index.inserts.lock().unwrap().is_empty());
    }

    #[test]
    fn backfill_does_not_dispatch_building_namespace_to_ordinary_postings() {
        let store = federated_store();
        let index = RecordingIndex::new();
        let vid = store.insert_vertex().expect("vertex");
        let label = VertexLabelId::from_raw(1);
        let vertex = store.vertex(vid).expect("vertex row");
        store
            .set_vertex_labels(vid, vertex, [label])
            .expect("label");
        crate::index::label_pending::clear_pending();
        let property = PropertyId::from_raw(6);
        let physical = PhysicalIndexId::new(903).expect("test physical id");
        let decoy_physical = PhysicalIndexId::new(904).expect("decoy physical id");
        // The index-build fence admits Building memberships into the Memory46 outbox, so the
        // namespace must carry a registered Building scope (exact catalog epoch) for the write.
        crate::index::canonical_export::register_scope(
            physical,
            gleaph_graph_kernel::canonical_export::CanonicalExportScope {
                graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(7),
                index_name_id: gleaph_graph_kernel::entry::IndexNameId::from_raw(9),
                catalog_epoch: 22,
                target: gleaph_graph_kernel::canonical_export::CanonicalExportTarget::Vertex {
                    label_id: 1,
                    property_id: property,
                    record_source: None,
                },
                inline: None,
            },
            candid::Principal::from_slice(&[0x5E, 0x11]),
        )
        .expect("register building scope");
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                IndexedVertexMembership {
                    physical_index_id: physical,
                    catalog_epoch: 22,
                    phase: IndexMaintenancePhase::Building,
                    property_id: property.raw(),
                    label_id: 1,
                    field_path: String::new(),
                    ancestor_property_id: 0,
                },
                IndexedVertexMembership {
                    physical_index_id: decoy_physical,
                    catalog_epoch: 22,
                    phase: IndexMaintenancePhase::Active,
                    property_id: property.raw(),
                    label_id: 2,
                    field_path: String::new(),
                    ancestor_property_id: 0,
                },
            ],
            ..Default::default()
        });
        store
            .set_vertex_property(vid, property, Value::Int64(7))
            .expect("property");
        crate::index::pending::clear_pending();
        let outbox_len = store.derived_index_outbox_len();
        assert_eq!(
            outbox_len, 1,
            "the Building envelope must be admitted to the Memory46 outbox"
        );
        // Acknowledge the admitted envelope so the scope reaches drained == admitted and can be
        // removed cleanly (the outbox entry itself is process-local test state).
        crate::index::canonical_export::ack_build_dml(physical, 22, 1)
            .expect("ack admitted envelope");
        crate::index::canonical_export::remove_scope(
            physical,
            &gleaph_graph_kernel::canonical_export::CanonicalExportScope {
                graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(7),
                index_name_id: gleaph_graph_kernel::entry::IndexNameId::from_raw(9),
                catalog_epoch: 22,
                target: gleaph_graph_kernel::canonical_export::CanonicalExportTarget::Vertex {
                    label_id: 1,
                    property_id: property,
                    record_source: None,
                },
                inline: None,
            },
        )
        .expect("cleanup building scope");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");
        assert_eq!(result.postings_synced, 0);
        assert!(index.inserts.lock().unwrap().is_empty());
        assert!(index.batches.lock().unwrap().is_empty());
    }

    #[test]
    fn backfill_batches_multiple_vertex_properties() {
        let store = federated_store();
        let index = RecordingIndex::batch();
        let vid = store.insert_vertex().expect("vertex");
        let name = crate::test_labels::property_id_for_name("batch_name");
        let score = crate::test_labels::property_id_for_name("batch_score");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[name, score]);
        store
            .set_vertex_property(vid, name, Value::Int64(42))
            .expect("name");
        store
            .set_vertex_property(vid, score, Value::Int64(99))
            .expect("score");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert_eq!(result.postings_synced, 2);
        let batches = index.batches.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 2);
    }

    #[test]
    fn backfill_continues_after_partial_batch_progress() {
        let store = federated_store();
        let index = RecordingIndex::batch_with_limit(1);
        let vid = store.insert_vertex().expect("vertex");
        let first = crate::test_labels::property_id_for_name("partial_first");
        let second = crate::test_labels::property_id_for_name("partial_second");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[first, second]);
        store
            .set_vertex_property(vid, first, Value::Int64(1))
            .expect("first");
        store
            .set_vertex_property(vid, second, Value::Int64(2))
            .expect("second");

        pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        let batches = index.batches.lock().unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn backfill_enumerates_record_leaves_alongside_flat_values() {
        let store = federated_store();
        let index = RecordingIndex::new();
        // Vertex A: one flat indexed value plus a record carrying two declared leaves.
        let flat_vertex = store.insert_vertex().expect("vertex");
        let flat_property = crate::test_labels::property_id_for_name("nested_backfill_flat");
        let stats = PropertyId::from_raw(70);
        let score_leaf = PropertyId::from_raw(71);
        let depth_leaf = PropertyId::from_raw(72);
        store
            .set_vertex_property(flat_vertex, flat_property, Value::Int64(5))
            .expect("flat value");
        store
            .set_vertex_property(
                flat_vertex,
                stats,
                Value::Record(vec![
                    ("score".to_owned(), Value::Int64(9)),
                    (
                        "meta".to_owned(),
                        Value::Record(vec![("depth".to_owned(), Value::Int64(11))]),
                    ),
                ]),
            )
            .expect("record value");
        // Vertex B: absence shapes only — missing root, non-record node, container leaf.
        let absent_vertex = store.insert_vertex().expect("vertex");
        store
            .set_vertex_property(absent_vertex, PropertyId::from_raw(73), Value::Int64(1))
            .expect("unrelated record");
        store
            .set_vertex_property(absent_vertex, stats, Value::Int64(2))
            .expect("non-record root");
        crate::index::pending::clear_pending();

        let nested = |physical: u64, leaf: PropertyId, field_path: &str| IndexedVertexMembership {
            physical_index_id: PhysicalIndexId::new(physical).expect("test physical id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            property_id: leaf.raw(),
            label_id: 0,
            field_path: field_path.to_owned(),
            ancestor_property_id: stats.raw(),
        };
        let flat_membership = IndexedVertexMembership {
            physical_index_id: PhysicalIndexId::new(900).expect("test physical id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            property_id: flat_property.raw(),
            label_id: 0,
            field_path: String::new(),
            ancestor_property_id: 0,
        };
        let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
            vertex_indexes: vec![
                flat_membership,
                nested(901, score_leaf, "stats.score"),
                nested(902, depth_leaf, "stats.meta.depth"),
            ],
            ..Default::default()
        });

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done);
        let inserts = index.inserts.lock().unwrap().clone();
        let key = |v: i64| {
            crate::property::sortable_index_key(&Value::Int64(v)).expect("int64 indexable")
        };
        let mut posted: Vec<(u64, u32, Vec<u8>)> = inserts
            .into_iter()
            .map(|(_, physical, property_id, payload, _)| (physical.raw(), property_id, payload))
            .collect();
        posted.sort();
        assert_eq!(
            posted,
            vec![
                (900, flat_property.raw(), key(5)),
                (901, score_leaf.raw(), key(9)),
                (902, depth_leaf.raw(), key(11)),
            ],
            "backfill must post flat values and every declared record leaf exactly once"
        );
    }

    #[test]
    fn backfill_propagates_batch_failure_without_success_result() {
        let store = federated_store();
        let index = RecordingIndex::batch_failure();
        let vid = store.insert_vertex().expect("vertex");
        let property = crate::test_labels::property_id_for_name("failed_batch");
        let _catalog = crate::index::catalog_context::enter_vertex_indexed(&[property]);
        store
            .set_vertex_property(vid, property, Value::Int64(7))
            .expect("property");

        let result = pollster::block_on(backfill_vertex_property_postings(
            &store,
            &index,
            PostingBackfillArgs {
                start_vertex_id: 0,
                max_vertices: 10,
            },
        ));
        assert!(result.is_err());
    }
}
