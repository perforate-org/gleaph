use super::*;
use crate::facade::stable::{
    IVF_CENTROID_META, IVF_CENTROIDS, PAGE_STORE, VECTOR_GC_CURSOR, VECTOR_MAINTENANCE_STATE,
    VECTOR_PARTITION_HEADS, VECTOR_REBUILD_STATE, VECTOR_SHARD_WATERMARKS, subject_store,
};
use crate::init::{DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED};
use crate::records::{IvfCentroidMeta, PartitionKey, RawMaintenanceState, RawRebuildState};
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorEncoding, VectorMetric, VectorSubject,
};

const INDEX_ID: u32 = 71;

fn router() -> Principal {
    Principal::from_slice(&[91])
}

fn shard() -> Principal {
    Principal::from_slice(&[92])
}

fn fixture() -> VectorCanisterStore {
    let store = VectorCanisterStore::new();
    store
        .reset_for_test_or_bench(&VectorCanisterInitArgs {
            router_canister: router(),
            definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
            subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
        })
        .expect("fixture reset");
    store.attach_single_shard_for_test(router(), ShardId::new(0), shard());
    store
}

fn upsert(store: &VectorCanisterStore) {
    store
        .vector_upsert(
            shard(),
            &VectorEmbeddingSyncOp {
                index_id: INDEX_ID,
                embedding_name_id: 0,
                subject: VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 1,
                },
                mutation_id: 1,
                encoding: VectorEncoding::F32,
                dims: 4,
                metric: VectorMetric::L2Squared,
                bytes: vec![0; 16],
                remove: false,
            },
        )
        .expect("seed definition-dependent state");
}

#[test]
fn busy_coupled_handle_rejects_before_definition_write() {
    let store = fixture();
    upsert(&store);
    let incarnation = definition_store::incarnation_for_test_or_bench().expect("incarnation");
    let subject_incarnation =
        subject_store::incarnation_for_test_or_bench().expect("subject incarnation");
    let definition = store.def_for_test(INDEX_ID).expect("definition");

    IVF_CENTROID_META.with(|centroid_meta| {
        let _busy = centroid_meta.borrow_mut();
        assert_eq!(
            store.reset_definition_domain(incarnation, subject_incarnation),
            Err(DefinitionDomainResetError::RegionHandleUnavailable(
                "IVF_CENTROID_META"
            ))
        );
    });

    assert_eq!(store.def_for_test(INDEX_ID), Some(definition));
    assert!(
        store
            .subject_entry_for_test(
                INDEX_ID,
                VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 1,
                }
            )
            .is_some()
    );
    assert_eq!(
        definition_store::incarnation_for_test_or_bench().expect("unchanged incarnation"),
        incarnation
    );
}

#[test]
fn successful_reset_clears_coupled_state_and_preserves_independent_lifecycle_state() {
    let store = fixture();
    upsert(&store);
    let partition = PartitionKey::new(INDEX_ID, 0, 0);
    IVF_CENTROID_META.with_borrow_mut(|meta| {
        meta.insert(
            INDEX_ID,
            IvfCentroidMeta {
                centroid_ready: true,
                centroid_epoch: 3,
                trained_index_version: 0,
            },
        );
    });
    IVF_CENTROIDS.with_borrow_mut(|centroids| {
        centroids.insert(partition, vec![1, 2, 3, 4]);
    });
    VECTOR_REBUILD_STATE.with_borrow_mut(|state| {
        state.insert(INDEX_ID, RawRebuildState(vec![1]));
    });
    VECTOR_MAINTENANCE_STATE.with_borrow_mut(|state| {
        state.insert(INDEX_ID, RawMaintenanceState(vec![2]));
    });
    let watermark_before = crate::records::ShardWatermarks {
        graph_watermark: 11,
        router_watermark: 7,
    };
    VECTOR_SHARD_WATERMARKS
        .with_borrow_mut(|watermarks| watermarks.insert(ShardId::new(0), watermark_before));
    let cursor_before = DeletedSubjectKey::new(
        ShardId::new(0),
        5,
        SubjectKey::new(
            INDEX_ID,
            VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 99,
            },
        ),
    );
    VECTOR_GC_CURSOR.with_borrow_mut(|cursor| cursor.set(Some(cursor_before)));

    let incarnation = definition_store::incarnation_for_test_or_bench().expect("incarnation");
    let subject_incarnation =
        subject_store::incarnation_for_test_or_bench().expect("subject incarnation");
    assert_eq!(
        store
            .reset_definition_domain(incarnation, subject_incarnation)
            .expect("coordinated reset"),
        incarnation + 1
    );

    assert!(store.def_for_test(INDEX_ID).is_none());
    assert!(IVF_CENTROID_META.with_borrow(|state| state.is_empty()));
    assert!(IVF_CENTROIDS.with_borrow(|state| state.is_empty()));
    assert!(subject_store::is_empty_for_test().expect("subject map empty"));
    assert!(VECTOR_DELETED_SUBJECTS.with_borrow(|state| state.is_empty()));
    assert!(
        VECTOR_PARTITION_HEADS
            .with_borrow(|state| state.is_empty())
            .expect("partition heads empty")
    );
    assert!(VECTOR_REBUILD_STATE.with_borrow(|state| state.is_empty()));
    assert!(VECTOR_MAINTENANCE_STATE.with_borrow(|state| state.is_empty()));
    assert_eq!(PAGE_STORE.with_borrow(|pages| pages.occupied_tail()), 32);

    assert_eq!(
        VECTOR_INDEX_ROUTER.with_borrow(|cell| *cell.get()),
        router()
    );
    assert_eq!(
        SHARD_CANISTER_CATALOG.with_borrow(|catalog| catalog.shard_for_canister(shard())),
        Some(ShardId::new(0))
    );
    assert_eq!(
        VECTOR_SHARD_WATERMARKS.with_borrow(|watermarks| watermarks.get(&ShardId::new(0))),
        Some(watermark_before)
    );
    assert_eq!(
        VECTOR_GC_CURSOR.with_borrow(|cursor| *cursor.get()),
        Some(cursor_before)
    );
}
