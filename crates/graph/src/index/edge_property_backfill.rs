//! Cursor-based backfill of edge property index postings from both canonical edge domains:
//! sidecar `EDGE_PROPERTIES` rows first, then canonical edges carrying indexed inline
//! property bytes, under one opaque cursor protocol.

use crate::facade::GraphStore;
use crate::index::lookup::{PropertyIndexLookup, dispatch_posting_batch};
use crate::property::sortable_index_key;
use gleaph_graph_kernel::federation::{EdgePostingBackfillArgs, EdgePostingBackfillResult};
use gleaph_graph_kernel::index::IndexPostingMutation;

/// Cursor domain byte: sidecar `EDGE_PROPERTIES` scan position.
const CURSOR_SIDE_DOMAIN: u8 = 0x00;
/// Cursor domain byte: canonical inline-property edge enumeration position.
const CURSOR_INLINE_DOMAIN: u8 = 0x01;

enum BackfillCursor {
    /// Raw `EdgePropertyKey` bytes of the next sidecar row.
    Side(Vec<u8>),
    /// Inline domain starts from its smallest candidate position.
    InlineFresh,
    /// Inclusive `(wire label id, owner vertex raw)` resume position.
    InlineResume(u16, u32),
    /// Both domains fully enumerated; the terminal result is due.
    InlineDone,
}

fn encode_side_cursor(key_bytes: Vec<u8>) -> Vec<u8> {
    let mut cursor = Vec::with_capacity(1 + key_bytes.len());
    cursor.push(CURSOR_SIDE_DOMAIN);
    cursor.extend_from_slice(&key_bytes);
    cursor
}

fn encode_inline_cursor(wire_label_id: u16, owner_vertex_raw: u32) -> Vec<u8> {
    let mut cursor = Vec::with_capacity(7);
    cursor.push(CURSOR_INLINE_DOMAIN);
    cursor.extend_from_slice(&wire_label_id.to_le_bytes());
    cursor.extend_from_slice(&owner_vertex_raw.to_le_bytes());
    cursor
}

fn decode_cursor(after_key: Option<Vec<u8>>) -> Result<BackfillCursor, String> {
    let Some(bytes) = after_key else {
        return Ok(BackfillCursor::Side(Vec::new()));
    };
    match bytes.split_first() {
        Some((&CURSOR_SIDE_DOMAIN, key)) => {
            if key.len() != 14 {
                return Err(format!(
                    "invalid sidecar backfill cursor payload length {}",
                    key.len()
                ));
            }
            Ok(BackfillCursor::Side(key.to_vec()))
        }
        Some((&CURSOR_INLINE_DOMAIN, payload)) => {
            if payload.len() != 6 {
                return Err(format!(
                    "invalid inline backfill cursor payload length {}",
                    payload.len()
                ));
            }
            let wire_label_id = u16::from_le_bytes([payload[0], payload[1]]);
            let owner_vertex_raw =
                u32::from_le_bytes([payload[2], payload[3], payload[4], payload[5]]);
            Ok(BackfillCursor::InlineResume(
                wire_label_id,
                owner_vertex_raw,
            ))
        }
        _ => Err(format!(
            "unrecognized backfill cursor domain {:?}",
            bytes.first()
        )),
    }
}

async fn emit_edge_posting_inserts(
    index: &dyn PropertyIndexLookup,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    mutations: &mut Vec<IndexPostingMutation>,
    physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId,
    property_id: u32,
    payload_bytes: Vec<u8>,
    label_id: u16,
    owner_vertex_raw: u32,
    slot_index: u32,
) -> Result<(), String> {
    if index.supports_posting_batch() {
        mutations.push(IndexPostingMutation::EdgeProperty {
            physical_index_id,
            remove: false,
            property_id,
            value: payload_bytes.clone(),
            label_id,
            owner_vertex_id: owner_vertex_raw,
            slot_index,
        });
    } else {
        index
            .edge_posting_insert_at(
                shard_id,
                physical_index_id,
                property_id,
                payload_bytes,
                label_id,
                owner_vertex_raw,
                slot_index,
            )
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn flush_posting_batch(
    index: &dyn PropertyIndexLookup,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    index_batch: &mut Vec<IndexPostingMutation>,
) -> Result<(), String> {
    if !index_batch.is_empty() {
        let batch = std::mem::take(index_batch);
        dispatch_posting_batch(index, shard_id, batch)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

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
    let mut cursor = decode_cursor(args.after_key.clone())?;
    let mut entries_processed = 0u32;
    let mut postings_synced = 0u32;
    let mut index_batch = Vec::new();

    'drive: loop {
        match cursor {
            BackfillCursor::Side(mut raw_key) => {
                // The side domain always runs first in a call, so the full budget is open.
                let batch = store.scan_edge_properties_batch(
                    (!raw_key.is_empty()).then(|| raw_key.clone()),
                    args.max_entries,
                )?;
                let batch_len_filled_budget = batch.len() as u32 >= args.max_entries;
                entries_processed = entries_processed.saturating_add(batch.len() as u32);
                for (key, value) in batch {
                    // Advance past every scanned row before membership filtering so the
                    // cursor never re-scans a skipped row on resume.
                    raw_key = GraphStore::edge_property_cursor(key);
                    let physical_index_ids =
                        crate::index::catalog_context::active_edge_physical_index_ids(
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
                        emit_edge_posting_inserts(
                            index,
                            shard_id,
                            &mut index_batch,
                            physical_index_id,
                            key.property_id().raw(),
                            payload_bytes.clone(),
                            key.label_id(),
                            owner_raw,
                            key.slot_index(),
                        )
                        .await?;
                        postings_synced = postings_synced.saturating_add(1);
                    }
                }
                if batch_len_filled_budget {
                    flush_posting_batch(index, shard_id, &mut index_batch).await?;
                    return Ok(EdgePostingBackfillResult {
                        next_after_key: Some(encode_side_cursor(raw_key)),
                        entries_processed,
                        postings_synced,
                        done: false,
                    });
                }
                // Sidecar domain exhausted under budget: chain into the inline domain within
                // the same call so one export API converges both canonical value stores.
                cursor = BackfillCursor::InlineFresh;
                continue 'drive;
            }
            BackfillCursor::InlineFresh => {
                cursor = inline_phase(
                    store,
                    index,
                    shard_id,
                    &mut index_batch,
                    None,
                    args.max_entries - entries_processed.min(args.max_entries),
                    &mut entries_processed,
                    &mut postings_synced,
                )
                .await?;
            }
            BackfillCursor::InlineResume(wire, owner) => {
                cursor = inline_phase(
                    store,
                    index,
                    shard_id,
                    &mut index_batch,
                    Some((wire, owner)),
                    args.max_entries,
                    &mut entries_processed,
                    &mut postings_synced,
                )
                .await?;
            }
            BackfillCursor::InlineDone => break 'drive,
        }
    }

    Ok(EdgePostingBackfillResult {
        next_after_key: None,
        entries_processed,
        postings_synced,
        done: true,
    })
}

/// Runs one inline-domain scan step and returns the cursor state to continue from:
/// `InlineResume` with the first unstarted position when the budget stopped enumeration,
/// or `InlineDone` when every candidate edge was enumerated.
#[allow(clippy::too_many_arguments)]
async fn inline_phase(
    store: &GraphStore,
    index: &dyn PropertyIndexLookup,
    shard_id: gleaph_graph_kernel::federation::ShardId,
    index_batch: &mut Vec<IndexPostingMutation>,
    start: Option<(u16, u32)>,
    max_edges: u32,
    entries_processed: &mut u32,
    postings_synced: &mut u32,
) -> Result<BackfillCursor, String> {
    let (entries, resume) = store.scan_canonical_inline_property_edges(start, max_edges)?;
    *entries_processed = entries_processed.saturating_add(entries.len() as u32);
    for entry in entries {
        for (property_id, value) in &entry.values {
            for physical_index_id in crate::index::catalog_context::active_edge_physical_index_ids(
                entry.wire_label_id,
                *property_id,
            ) {
                let Some(payload_bytes) = sortable_index_key(value) else {
                    continue;
                };
                emit_edge_posting_inserts(
                    index,
                    shard_id,
                    index_batch,
                    physical_index_id,
                    property_id.raw(),
                    payload_bytes,
                    entry.wire_label_id,
                    entry.owner_vertex_raw,
                    entry.slot_index,
                )
                .await?;
                *postings_synced = postings_synced.saturating_add(1);
            }
        }
    }
    match resume {
        Some((wire, owner)) => {
            flush_posting_batch(index, shard_id, index_batch).await?;
            Ok(BackfillCursor::InlineResume(wire, owner))
        }
        None => {
            flush_posting_batch(index, shard_id, index_batch).await?;
            Ok(BackfillCursor::InlineDone)
        }
    }
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
        EdgeIndexDirection, IndexIntersectionRequest, IndexPostingBatchProgress,
        IndexPostingMutation, PhysicalIndexId, PostingHit, PostingRangeRequest,
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
                vector_canister: None,
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

    // --- inline domain (GAP-2026-07-29-001) ---

    const INLINE_BACKFILL_LABEL_RAW: u16 = 21;
    const INLINE_BACKFILL_PROPERTY_RAW: u32 = 921;

    fn install_inline_backfill_fixture(
        direction: EdgeIndexDirection,
    ) -> crate::index::catalog_context::CatalogGuard {
        use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
        let label = gleaph_graph_kernel::entry::EdgeLabelId::from_raw(INLINE_BACKFILL_LABEL_RAW);
        let property = PropertyId::from_raw(INLINE_BACKFILL_PROPERTY_RAW);
        crate::test_labels::install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 4,
                encoding: EdgeInlinePropertyEncoding::F32,
            },
        );
        crate::test_labels::install_test_edge_inline_property(label, property);
        crate::index::catalog_context::enter(gleaph_graph_kernel::index::IndexedPropertyCatalog {
            edge_indexes: vec![gleaph_graph_kernel::index::IndexedEdgeMembership {
                physical_index_id: PhysicalIndexId::new(121).expect("physical id"),
                catalog_epoch: 1,
                phase: gleaph_graph_kernel::index::IndexMaintenancePhase::Active,
                label_id: label.raw(),
                property_id: property.raw(),
                direction,
                field_path: String::new(),
            }],
            ..Default::default()
        })
    }

    fn weight_placeholder() -> PropertyId {
        PropertyId::from_raw(55)
    }

    fn sortable_f32(v: f32) -> Vec<u8> {
        crate::property::sortable_index_key(&Value::Float32(v)).expect("sortable key")
    }

    #[test]
    fn backfill_enumerates_indexed_inline_scalar_values() {
        let store = federated_store();
        let index = RecordingEdgeIndex::new();
        let _catalog = install_inline_backfill_fixture(EdgeIndexDirection::Outgoing);
        let source = store.insert_vertex().expect("source");
        let target = store.insert_vertex().expect("target");
        let label = gleaph_graph_kernel::entry::EdgeLabelId::from_raw(INLINE_BACKFILL_LABEL_RAW);
        store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                target,
                Some(label),
                &1.5f32.to_le_bytes(),
            )
            .expect("edge");

        let result = pollster::block_on(backfill_edge_property_postings(
            &store,
            &index,
            EdgePostingBackfillArgs {
                after_key: None,
                max_entries: 10,
            },
        ))
        .expect("backfill");

        assert!(result.done, "inline exhaustion must complete the export");
        assert_eq!(result.postings_synced, 1);
        let inserts = index.inserts.lock().unwrap();
        let wire_label = label.pack(gleaph_graph_kernel::entry::EdgeDirectedness::Directed);
        assert_eq!(
            inserts.as_slice(),
            &[(
                0u32,
                INLINE_BACKFILL_PROPERTY_RAW,
                sortable_f32(1.5),
                wire_label.raw(),
                u32::from(source),
                0u32,
            )],
            "the insert must carry the exact canonical mutation identity"
        );
    }

    #[test]
    fn backfill_emits_only_the_canonical_undirected_owner() {
        let store = federated_store();
        let index = RecordingEdgeIndex::new();
        let _catalog = install_inline_backfill_fixture(EdgeIndexDirection::Undirected);
        let low = store.insert_vertex().expect("low");
        let high = store.insert_vertex().expect("high");
        assert!(u64::from(low) < u64::from(high), "fixture ordering");
        let label = gleaph_graph_kernel::entry::EdgeLabelId::from_raw(INLINE_BACKFILL_LABEL_RAW);
        store
            .insert_undirected_edge_with_inline_property_bytes(
                low,
                high,
                Some(label),
                &2.5f32.to_le_bytes(),
            )
            .expect("undirected edge");

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
        assert_eq!(result.postings_synced, 1, "mirrors must not double-post");
        let inserts = index.inserts.lock().unwrap();
        assert_eq!(inserts.len(), 1);
        assert_eq!(
            inserts[0].4,
            u32::from(high),
            "the undirected canonical owner is the max endpoint"
        );
    }

    #[test]
    fn backfill_resume_walks_both_domains_without_duplicate_identities() {
        let store = federated_store();
        let index = RecordingEdgeIndex::new();
        let _catalog = install_inline_backfill_fixture(EdgeIndexDirection::Outgoing);
        let source = store.insert_vertex().expect("source");
        let first = store.insert_vertex().expect("first");
        let second = store.insert_vertex().expect("second");
        // One sidecar-domain row and two inline-domain edges. The export resolves
        // memberships against ONE router-supplied catalog, so both domains' registrations
        // must live in that single catalog snapshot.
        let sidecar_label = crate::test_labels::edge_label_id_for_name("backfill_edge_label");
        let sidecar_handle = store
            .insert_directed_edge(source, first, Some(sidecar_label))
            .expect("sidecar edge");
        let inline_label =
            gleaph_graph_kernel::entry::EdgeLabelId::from_raw(INLINE_BACKFILL_LABEL_RAW);
        store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                first,
                Some(inline_label),
                &1.5f32.to_le_bytes(),
            )
            .expect("first inline edge");
        store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                second,
                Some(inline_label),
                &3.5f32.to_le_bytes(),
            )
            .expect("second inline edge");
        let _catalog = crate::index::catalog_context::enter(
            gleaph_graph_kernel::index::IndexedPropertyCatalog {
                edge_indexes: vec![
                    gleaph_graph_kernel::index::IndexedEdgeMembership {
                        physical_index_id: PhysicalIndexId::new(102).expect("physical id"),
                        catalog_epoch: 1,
                        phase: gleaph_graph_kernel::index::IndexMaintenancePhase::Active,
                        label_id: sidecar_label.raw(),
                        property_id: 55,
                        direction: EdgeIndexDirection::Any,
                        field_path: String::new(),
                    },
                    gleaph_graph_kernel::index::IndexedEdgeMembership {
                        physical_index_id: PhysicalIndexId::new(121).expect("physical id"),
                        catalog_epoch: 1,
                        phase: gleaph_graph_kernel::index::IndexMaintenancePhase::Active,
                        label_id: INLINE_BACKFILL_LABEL_RAW,
                        property_id: INLINE_BACKFILL_PROPERTY_RAW,
                        direction: EdgeIndexDirection::Outgoing,
                        field_path: String::new(),
                    },
                ],
                ..Default::default()
            },
        );
        store
            .set_edge_property(
                sidecar_handle.occurrence(ic_stable_lara::labeled::LabeledOrientation::Forward),
                weight_placeholder(),
                Value::Int64(9),
            )
            .expect("weight");

        let mut after_key = None;
        let mut all_inserts = Vec::new();
        for step in 0..8 {
            let result = pollster::block_on(backfill_edge_property_postings(
                &store,
                &index,
                EdgePostingBackfillArgs {
                    after_key: after_key.clone(),
                    max_entries: 1,
                },
            ))
            .expect("backfill step");
            all_inserts.extend(index.inserts.lock().unwrap().drain(..));
            if result.done {
                // The inline domain may overshoot its per-call budget by one vertex's
                // matching degree, so both edges can legitimately converge in one call.
                assert_eq!(result.next_after_key, None);
                break;
            }
            assert!(step < 7, "resume never converged");
            after_key = result.next_after_key;
        }
        let mut identities = all_inserts
            .iter()
            .map(|(_, property, value, wire, owner, slot)| {
                (*property, value.clone(), *wire, *owner, *slot)
            })
            .collect::<Vec<_>>();
        identities.sort();
        identities.dedup();
        assert_eq!(
            identities.len(),
            all_inserts.len(),
            "resume must not replay an identity: {all_inserts:?}"
        );
        assert_eq!(identities.len(), 3, "one sidecar + two inline postings");
    }

    #[test]
    fn unversioned_backfill_cursor_is_rejected() {
        let store = federated_store();
        let index = RecordingEdgeIndex::new();
        let err = pollster::block_on(backfill_edge_property_postings(
            &store,
            &index,
            EdgePostingBackfillArgs {
                after_key: Some(vec![0xFF; 14]),
                max_entries: 10,
            },
        ))
        .expect_err("legacy bare cursor must be rejected");
        assert!(err.contains("domain"), "got {err}");
    }
}
