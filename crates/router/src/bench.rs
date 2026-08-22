//! Phase 8 stable-memory layout benchmarks (ADR 0007 §6).
//!
//! Run from `crates/router`: `canbench` (see `canbench.yml`).

use crate::facade::stable::memory;
use crate::facade::stable::prepared_catalog::{
    PreparedPlanKey, PreparedPlanRecord, PreparedPlanRecordV1, insert_prepared_plan,
};
use canbench_rs::bench;
use std::hint::black_box;

fn router_stable_reopen_round() {
    // auth
    black_box(memory::init_auth_state());
    // registry
    black_box(memory::init_graphs());
    black_box(memory::init_shards());
    black_box(memory::init_shard_by_graph());
    black_box(memory::init_shards_by_graph_id());
    black_box(memory::init_graph_runtime_config());
    // idempotency / prepared queries
    black_box(memory::init_mutation_counter());
    black_box(memory::init_mutation_by_client_key());
    black_box(memory::init_prepared_plans());
    // catalog
    black_box(memory::init_vertex_label_catalog());
    black_box(memory::init_edge_label_catalog());
    black_box(memory::init_property_catalog());
    black_box(memory::init_graph_catalog());
    black_box(memory::init_index_name_catalog());
    black_box(memory::init_named_indexes());
    black_box(memory::init_next_physical_index_id());
    black_box(memory::init_edge_inline_property_profiles());
    black_box(memory::init_gql_graph_catalog());
    black_box(memory::init_graph_type_name_catalog());
    black_box(memory::init_constraint_name_catalog());
    black_box(memory::init_unique_constraints());
    black_box(memory::init_unique_reservations());
    black_box(memory::init_mutation_reservation_index());
    black_box(memory::init_unique_effect_pending());
    black_box(memory::init_embedding_name_catalog());
    black_box(memory::init_vector_indexes());
    black_box(memory::init_next_vector_index_id());
    black_box(memory::init_vector_ingest_outbox());
    black_box(memory::init_vector_dispatch_activation());
    black_box(memory::init_vector_maintenance_policies());
    // provisioning
    black_box(memory::init_provisioning_requests());
    black_box(memory::init_provisioning_by_graph());
    black_box(memory::init_provisioning_intent_locks());
    black_box(memory::init_provision_config());
    // durable bulk-load receipts (ADR 0057) and the index-catalog epoch fence (ADR 0059)
    black_box(memory::init_bulk_load_chunk_receipts());
    black_box(memory::init_schema_migrations());
    black_box(memory::init_index_catalog_epoch());
    // telemetry
    black_box(memory::init_vertex_label_stats());
    black_box(memory::init_edge_label_stats());
    black_box(memory::init_vertex_label_live_by_shard());
    black_box(memory::init_edge_label_live_by_shard());
    black_box(memory::init_label_stats_projection());
    // maintenance
    black_box(memory::init_label_backfill_state());
    black_box(memory::init_vertex_property_backfill_state());
    black_box(memory::init_edge_backfill_state());
}

#[bench(raw)]
fn bench_layout_router_stable_reopen_touch() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_router_reopen");
        router_stable_reopen_round();
    })
}

fn setup_vector_frontier_rows_1024(one_marker_per_lane: bool) {
    use crate::facade::stable::vector_ingest_outbox::{
        VectorIngestIntentPhase, VectorIngestOutboxKey, VectorIngestOutboxState,
        VectorIngestOutboxValue, MAX_VECTOR_INGEST_OUTBOX_ROWS, MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES,
    };
    use candid::Principal;
    use gleaph_graph_kernel::entry::{GraphId, VertexLabelId};
    use gleaph_graph_kernel::federation::{LocalVertexId, ShardId};
    use gleaph_graph_kernel::vector_index::{
        IndexedEmbeddingSpec, VectorEncoding, VectorIndexKind, VectorMetric,
    };

    let spec = IndexedEmbeddingSpec {
        embedding_name_id: 3,
        index_id: 7,
        kind: VectorIndexKind::IvfFlat,
        metric: VectorMetric::L2Squared,
        encoding: VectorEncoding::F32,
        dims: u16::MAX,
        labels: vec![VertexLabelId::from_raw(1)],
    };
    let payload_bytes = spec.encoding.stride_bytes(spec.dims) as usize;
    assert!(payload_bytes <= MAX_VECTOR_INGEST_OUTBOX_ROW_BYTES);
    crate::facade::stable::ROUTER_VECTOR_INGEST_OUTBOX.with_borrow_mut(|table| {
        table.clear_new();
        for mutation_id in 1..=MAX_VECTOR_INGEST_OUTBOX_ROWS as u64 {
            let vector_target = if one_marker_per_lane {
                let mut principal_bytes = [1u8; 29];
                principal_bytes[..8].copy_from_slice(&mutation_id.to_be_bytes());
                Principal::from_slice(&principal_bytes)
            } else {
                Principal::from_slice(&[1; 29])
            };
            let state = VectorIngestOutboxState {
                graph_id: GraphId::from_raw(1),
                graph_target: Principal::from_slice(&[9; 29]),
                vector_target,
                shard_id: ShardId::new(2),
                local_vertex_id: LocalVertexId::from(mutation_id as u32),
                spec: spec.clone(),
                mutation_id,
                // Match the public F32 ingestion contract: every row carries exactly the
                // canonical `encoding.stride_bytes(dims)` payload for the largest admissible
                // `u16` dimension. The frontier derivation remains key-only and never decodes
                // these embedding bytes.
                bytes: vec![0; payload_bytes],
                phase: if !one_marker_per_lane && mutation_id == 1024 {
                    VectorIngestIntentPhase::AwaitingVector
                } else {
                    VectorIngestIntentPhase::AwaitingFrontier
                },
            };
            let key = VectorIngestOutboxKey::from_state(&state);
            let value = VectorIngestOutboxValue::from_state(&state);
            assert!(table.insert(key, value).is_none());
        }
    });
    crate::facade::stable::ROUTER_MUTATION_COUNTER
        .with_borrow_mut(|counter| counter.set(MAX_VECTOR_INGEST_OUTBOX_ROWS as u64));
}

#[bench(raw)]
fn bench_router_frontier_key_only_scan_single_lane_1024() -> canbench_rs::BenchResult {
    setup_vector_frontier_rows_1024(false);
    let expected = crate::facade::stable::vector_ingest_outbox::derive_frontier_snapshots()
        .expect("bench frontier derivation");
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].frontier, 1023);
    assert_eq!(expected[0].marker_keys.len(), 1023);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("router_frontier_key_only_scan_single_lane_1024");
        let snapshots = crate::facade::stable::vector_ingest_outbox::derive_frontier_snapshots()
            .expect("frontier derivation");
        black_box(snapshots);
    })
}

#[bench(raw)]
fn bench_router_frontier_key_only_scan_1024_lanes_one_marker_each() -> canbench_rs::BenchResult {
    setup_vector_frontier_rows_1024(true);
    let expected = crate::facade::stable::vector_ingest_outbox::derive_frontier_snapshots()
        .expect("bench frontier derivation");
    assert_eq!(expected.len(), 1024);
    assert!(expected
        .iter()
        .all(|snapshot| snapshot.marker_keys.len() == 1));
    canbench_rs::bench_fn(|| {
        let _scope =
            canbench_rs::bench_scope("router_frontier_key_only_scan_1024_lanes_one_marker_each");
        let snapshots = crate::facade::stable::vector_ingest_outbox::derive_frontier_snapshots()
            .expect("frontier derivation");
        black_box(snapshots);
    })
}

fn setup_markerless_frontier_catalog() -> crate::facade::stable::graph_catalog::AttachedVectorLane {
    use crate::facade::stable::graph_catalog::VECTOR_LANE_CATALOG_PAGE_BUDGET;
    use candid::Principal;
    use gleaph_graph_kernel::entry::GraphId;
    use gleaph_graph_kernel::federation::{GraphShardKey, ShardId, ShardRegistryEntry};

    setup_vector_frontier_rows_1024(false);
    assert_eq!(VECTOR_LANE_CATALOG_PAGE_BUDGET, 64);

    let vector_target = Principal::from_slice(&[2; 29]);
    let selected_shard = ShardId::new((VECTOR_LANE_CATALOG_PAGE_BUDGET - 1) as u32);
    crate::facade::stable::ROUTER_SHARDS.with_borrow_mut(|shards| {
        shards.clear_new();
        for ordinal in 0..VECTOR_LANE_CATALOG_PAGE_BUDGET {
            let graph_id = GraphId::from_raw(980_000 + ordinal as u32);
            let shard_id = ShardId::new(ordinal as u32);
            let fully_attached = shard_id == selected_shard;
            let key = GraphShardKey::new(graph_id, shard_id);
            let entry = ShardRegistryEntry {
                shard_id,
                graph_canister: Principal::from_slice(&[(ordinal + 1) as u8; 29]),
                index_canister: Principal::management_canister(),
                graph_id,
                registered_at_ns: 0,
                index_attached: fully_attached,
                vector_canister: fully_attached.then_some(vector_target),
                vector_index_attached: fully_attached,
            };
            assert!(shards.insert(key, entry.into()).is_none());
        }
    });

    crate::facade::stable::graph_catalog::AttachedVectorLane {
        vector_target,
        shard_id: selected_shard,
    }
}

fn markerless_frontier_catalog_round() -> Result<
    (
        crate::facade::stable::graph_catalog::AttachedVectorLanePage,
        crate::facade::stable::vector_ingest_outbox::VectorFrontierSnapshot,
    ),
    String,
> {
    let page = crate::facade::stable::graph_catalog::scan_attached_vector_lane(None)
        .map_err(|error| error.to_string())?;
    let lane = page
        .lane
        .ok_or_else(|| "markerless frontier catalog page has no attached lane".to_owned())?;
    let snapshot = crate::facade::stable::vector_ingest_outbox::derive_frontier_snapshot_for_lane(
        lane.vector_target,
        lane.shard_id,
    )?;
    Ok((page, snapshot))
}

#[bench(raw)]
fn markerless_frontier_catalog() -> canbench_rs::BenchResult {
    use crate::facade::stable::graph_catalog::VECTOR_LANE_CATALOG_PAGE_BUDGET;
    use crate::facade::stable::vector_ingest_outbox::MAX_VECTOR_INGEST_OUTBOX_ROWS;

    let expected_lane = setup_markerless_frontier_catalog();
    let (page, snapshot) =
        markerless_frontier_catalog_round().expect("derive markerless catalog frontier");
    assert_eq!(page.scanned as usize, VECTOR_LANE_CATALOG_PAGE_BUDGET);
    assert_eq!(page.lane, Some(expected_lane));
    assert_eq!(snapshot.vector_target, expected_lane.vector_target);
    assert_eq!(snapshot.shard_id, expected_lane.shard_id);
    assert_eq!(snapshot.frontier, MAX_VECTOR_INGEST_OUTBOX_ROWS as u64);
    assert!(snapshot.marker_keys.is_empty());

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("markerless_frontier_catalog");
        let _ = black_box(markerless_frontier_catalog_round());
    })
}

// -----------------------------------------------------------------------------
// Initial per-memory bucket policy capacity probes.
//
// These are growth probes, not maximum-capacity tests. They intentionally use the production
// catalog APIs and distinct keyspaces so the stable-memory delta shows how much extent capacity
// each policy class consumes for representative Router rows.
// -----------------------------------------------------------------------------

#[bench(raw)]
fn bench_router_property_catalog_growth_1024() -> canbench_rs::BenchResult {
    let graph_id = GraphId::from_raw(970_001);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("router_property_catalog_growth_1024");
        for i in 0..1024u32 {
            let name = format!("capacity-property-{i:04}");
            RouterStore::commit_intern_property_name(black_box(graph_id), black_box(name.as_str()))
                .expect("intern property name");
        }
    })
}

#[bench(raw)]
fn bench_router_prepared_plan_growth_32x256k() -> canbench_rs::BenchResult {
    let graph_id = GraphId::from_raw(970_002);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("router_prepared_plan_growth_32x256k");
        for i in 0..32u32 {
            insert_prepared_plan(
                PreparedPlanKey::new(format!("capacity-plan-{i:02}")),
                PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
                    graph_id,
                    query: black_box("MATCH (n) RETURN n".to_string()),
                    metadata: None,
                }),
            );
        }
    })
}

/// ADR 0061 batch-registration cost: plan, complete metadata, and commit a full 32-operation
/// batch through the production batch core. Setup (admin grant + home-graph registration) stays
/// outside the measured closure; each iteration re-upserts the same keys, so the stable map stays
/// bounded and the measurement reflects registration cost, not storage growth.
#[bench(raw)]
fn bench_router_prepared_batch_register_32() -> canbench_rs::BenchResult {
    use candid::Principal;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};
    use gleaph_prepared_api::{
        OperationKind, PreparedOperation, PreparedRegistration, ResultSchema,
    };

    let caller = Principal::from_slice(&[0xAB; 29]);
    crate::facade::auth::grant_admins(&[caller]);
    let store = RouterStore::new();
    store
        .admin_register_graph(
            caller,
            GraphRegistryEntry {
                graph_id: GraphId::from_raw(970_003),
                canister_id: Principal::management_canister(),
                owner: caller,
                admins: Default::default(),
                status: GraphStatus::Active,
                version: 1,
                updated_at_ns: 0,
                provisioning_state: ProvisioningState::None,
                is_home: true,
            },
            "bench-prepared-batch",
        )
        .expect("register bench graph");
    let operations: Vec<PreparedRegistration> = (0..32)
        .map(|index| PreparedRegistration {
            name: format!("bench-batch-{index:02}"),
            query: "MATCH (n) RETURN n".to_string(),
            metadata: Some(PreparedOperation {
                name: format!("bench-batch-{index:02}"),
                description: None,
                kind: OperationKind::Query,
                parameters: vec![],
                result: ResultSchema { columns: vec![] },
                supports_consistency: false,
                supports_idempotency: false,
                allowed_sorts: vec![],
            }),
        })
        .collect();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("router_prepared_batch_register_32");
        crate::prepared::prepare_batch_core(&operations, caller).expect("batch registers");
    })
}

// ----------------------------------------------------------------------------
// ADR 0030 cross-shard uniqueness write-path benchmarks (Router-side only).
//
// SCOPE / GATE STATUS: these measure the **Router-local** cost of the reservation TCC and the
// slice-6 recovery indexes, exercised through the production facade ([`RouterStore`]) so each
// op includes the reverse-index work the real write path does:
//   - Try: `try_reserve_unique` — the no-`await` conflict scan, reservation insert, **and** the
//     `MutationId → {client_key, nonterminal}` reverse-index slot bump (at 1/16/256 claims);
//   - Confirm: `confirm_unique_claim` (`Reserved → Committed`) **plus** the
//     `release_unique_reservation_slot` non-terminal count decrement on `FreshlyCommitted`;
//   - Cancel: `cancel_reclaim` under the reclaim fence **plus** the count decrement;
//   - `clear_unique_acquire_ack`: the Router-local `pending_acquire_ack` unpin marker;
//   - the bounded reclaim scan over a populated table.
//
// These do **not** measure the inter-canister legs of the protocol: the graph-shard unique-effect
// **outbox append/ack round** and the canonical write are cross-canister and cannot be exercised
// from a Router-only canbench; the facade Cancel's `RouterMutationRecord` terminal-failure write is
// a journal-record cost, not a reservation-table cost. The ADR 0030 Phase-6 canbench gate
// (Try/Confirm/Cancel overhead **and** the outbox ack round **and** reservation-table storage
// growth, end to end) is therefore **not** fully satisfied by these alone — see the ADR gate note.
//
// Each bench uses a distinct `graph_id`/`mutation_id` so the shared thread-local tables do not
// collide across benches in the same canister instance.
// ----------------------------------------------------------------------------

use crate::facade::stable::reservation_catalog::{
    ConfirmOutcome, begin_reclaim, cancel_reclaim, scan_reclaim_candidates,
};
use crate::facade::store::RouterStore;
use crate::federation::ShardDispatch;
use gleaph_graph_kernel::entry::{ConstraintNameId, GraphId};
use gleaph_graph_kernel::federation::{ClaimId, EffectId, ShardId};
use gleaph_graph_kernel::plan_exec::UniqueClaimDispatch;

const BENCH_CONSTRAINT: ConstraintNameId = ConstraintNameId::from_raw(1);

fn bench_caller() -> candid::Principal {
    candid::Principal::anonymous()
}

fn bench_claims(count: u32) -> Vec<UniqueClaimDispatch> {
    (0..count)
        .map(|i| UniqueClaimDispatch {
            claim_ordinal: i,
            constraint_id: BENCH_CONSTRAINT,
            encoded_value: format!("bench-value-{i:08}").into_bytes(),
        })
        .collect()
}

fn bench_dispatch() -> Vec<ShardDispatch> {
    vec![ShardDispatch {
        shard_id: ShardId::new(0),
        graph_canister: candid::Principal::anonymous(),
        seed_bindings_blob: None,
        resolved_search_blob: None,
    }]
}

/// Seed `count` `Reserved` entries (one mutation's claim set) through the production facade, so the
/// reverse-index slot is bumped exactly as on the live write path.
fn seed_reserved(store: &RouterStore, graph: GraphId, mutation_id: u64, count: u32) {
    let claims = bench_claims(count);
    store
        .try_reserve_unique(
            bench_caller(),
            graph,
            mutation_id,
            "bench-key",
            &claims,
            &bench_dispatch(),
        )
        .expect("bench seed try_reserve_unique");
}

fn bench_try_reserve(
    graph_seed: u32,
    claim_count: u32,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = RouterStore::new();
    let graph = GraphId::from_raw(910_000 + graph_seed);
    let mutation_id = 7_000_000 + graph_seed as u64;
    let claims = bench_claims(claim_count);
    let dispatch = bench_dispatch();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        store
            .try_reserve_unique(
                black_box(bench_caller()),
                black_box(graph),
                black_box(mutation_id),
                black_box("bench-key"),
                black_box(&claims),
                black_box(&dispatch),
            )
            .expect("bench try_reserve_unique");
    })
}

#[bench(raw)]
fn bench_unique_try_reserve_1() -> canbench_rs::BenchResult {
    bench_try_reserve(1, 1, "unique_try_reserve_1")
}

#[bench(raw)]
fn bench_unique_try_reserve_16() -> canbench_rs::BenchResult {
    bench_try_reserve(16, 16, "unique_try_reserve_16")
}

#[bench(raw)]
fn bench_unique_try_reserve_256() -> canbench_rs::BenchResult {
    bench_try_reserve(256, 256, "unique_try_reserve_256")
}

#[bench(raw)]
fn bench_unique_confirm_reservation() -> canbench_rs::BenchResult {
    let store = RouterStore::new();
    let graph = GraphId::from_raw(920_001);
    let mutation_id = 7_200_001;
    seed_reserved(&store, graph, mutation_id, 1);
    let claim = bench_claims(1).remove(0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_confirm_reservation");
        let outcome = store.confirm_unique_claim(
            black_box(graph),
            black_box(mutation_id),
            black_box(&claim),
            black_box(vec![9u8; 16]),
            black_box(EffectId::new(mutation_id, 0)),
        );
        // The live caller decrements the non-terminal count only on the fresh transition.
        if matches!(outcome, ConfirmOutcome::FreshlyCommitted) {
            store.release_unique_reservation_slot(black_box(mutation_id));
        }
        black_box(outcome);
    })
}

#[bench(raw)]
fn bench_unique_cancel_reclaim() -> canbench_rs::BenchResult {
    let store = RouterStore::new();
    let graph = GraphId::from_raw(950_001);
    let mutation_id = 7_500_001;
    seed_reserved(&store, graph, mutation_id, 1);
    let value = bench_claims(1).remove(0).encoded_value;
    let ticket = begin_reclaim(graph, BENCH_CONSTRAINT, &value).expect("bench begin_reclaim");
    let claim_id = ClaimId::new(mutation_id, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_cancel_reclaim");
        let removed = cancel_reclaim(
            black_box(graph),
            black_box(BENCH_CONSTRAINT),
            black_box(&value),
            black_box(claim_id),
            black_box(ticket.generation),
        );
        store.release_unique_reservation_slot(black_box(mutation_id));
        black_box(removed);
    })
}

#[bench(raw)]
fn bench_unique_clear_acquire_ack() -> canbench_rs::BenchResult {
    let store = RouterStore::new();
    let graph = GraphId::from_raw(930_001);
    let mutation_id = 7_300_001;
    seed_reserved(&store, graph, mutation_id, 1);
    let claim = bench_claims(1).remove(0);
    let value = claim.encoded_value.clone();
    // Commit so a pending ack exists to clear (the slice-6 unpin path).
    let _ = store.confirm_unique_claim(
        graph,
        mutation_id,
        &claim,
        vec![9u8; 16],
        EffectId::new(mutation_id, 0),
    );
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_clear_acquire_ack");
        let cleared = store.clear_unique_acquire_ack(
            black_box(graph),
            black_box(BENCH_CONSTRAINT),
            black_box(&value),
            black_box(ClaimId::new(mutation_id, 0)),
        );
        black_box(cleared);
    })
}

#[bench(raw)]
fn bench_unique_reclaim_scan_256() -> canbench_rs::BenchResult {
    let store = RouterStore::new();
    let graph = GraphId::from_raw(940_001);
    seed_reserved(&store, graph, 7_400_001, 256);
    // All seeded `Reserved` entries are past the reclaim-eligibility TTL at this clock.
    let now = u64::MAX;
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("unique_reclaim_scan_256");
        let (candidates, _next, scanned) =
            scan_reclaim_candidates(black_box(None), black_box(256), black_box(now));
        black_box((candidates, scanned));
    })
}

// -----------------------------------------------------------------------------
// ADR 0034 Slice 20: inline edge scalar schema benchmarks.
// -----------------------------------------------------------------------------

use std::sync::atomic::{AtomicU32, Ordering};

static INLINE_BENCH_GRAPH_SEED: AtomicU32 = AtomicU32::new(1);

fn bench_inline_graph_id() -> gleaph_graph_kernel::entry::GraphId {
    gleaph_graph_kernel::entry::GraphId::from_raw(
        900_000 + INLINE_BENCH_GRAPH_SEED.fetch_add(1, Ordering::SeqCst),
    )
}

#[bench(raw)]
fn bench_inline_edge_scalar_schema_lookup() -> canbench_rs::BenchResult {
    let _store = RouterStore::new();
    let graph_id = bench_inline_graph_id();
    let label_id = RouterStore::commit_intern_edge_label_name(graph_id, "ROAD").expect("label");
    let property_id =
        RouterStore::commit_intern_property_name(graph_id, "distance").expect("property");
    crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow_mut(|s| {
            s.set_inline_scalar_schema(
                graph_id,
                label_id,
                property_id,
                crate::facade::stable::edge_inline_property_profiles::InlineScalarType::F32,
            )
        })
        .expect("seed inline schema");

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_schema_lookup");
        let profile = crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow(|s| s.get_profile(graph_id, label_id));
        black_box(profile);
    })
}

#[bench(raw)]
fn bench_inline_edge_scalar_schema_commit() -> canbench_rs::BenchResult {
    let graph_id = bench_inline_graph_id();
    // Commit the label and property once outside the measured closure so the benchmark measures
    // only the schema-record commit path.
    let label_id = RouterStore::commit_intern_edge_label_name(graph_id, "ROAD").expect("label");
    let property_id =
        RouterStore::commit_intern_property_name(graph_id, "distance").expect("property");

    canbench_rs::bench_fn(move || {
        let _scope = canbench_rs::bench_scope("inline_scalar_schema_commit");
        crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|s| {
                s.set_inline_scalar_schema(
                    graph_id,
                    label_id,
                    property_id,
                    crate::facade::stable::edge_inline_property_profiles::InlineScalarType::F32,
                )
            })
            .expect("commit inline schema");
    })
}

#[bench(raw)]
fn bench_inline_edge_struct_schema_commit() -> canbench_rs::BenchResult {
    use crate::facade::stable::edge_inline_property_profiles::{
        EdgeInlinePropertySchemaRecord, InlineScalarType, InlineStructLayout,
    };

    let graph_id = bench_inline_graph_id();
    // Commit the label and property once outside the measured closure so the benchmark measures
    // only the schema-record commit path.
    let label_id = RouterStore::commit_intern_edge_label_name(graph_id, "AFFINITY").expect("label");
    let property_id =
        RouterStore::commit_intern_property_name(graph_id, "stats").expect("property");
    let layout = InlineStructLayout::from_fields(vec![
        ("score".into(), InlineScalarType::F32),
        ("confidence".into(), InlineScalarType::F32),
        ("updated_at".into(), InlineScalarType::U64),
    ])
    .expect("seed layout");

    // Pre-measurement sanity: exercise the real store setter on a separate sanity label and
    // assert the persisted logical specs plus derived opaque profile. A no-op or broken setter
    // makes benchmark setup fail rather than measure garbage.
    let sanity_label_id = RouterStore::commit_intern_edge_label_name(graph_id, "AFFINITY_SANITY")
        .expect("sanity label");
    let sanity_property_id = RouterStore::commit_intern_property_name(graph_id, "stats_sanity")
        .expect("sanity property");
    crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow_mut(|s| {
            s.set_inline_struct_schema(
                graph_id,
                sanity_label_id,
                sanity_property_id,
                layout.clone(),
            )
        })
        .expect("sanity commit inline struct schema");
    let sanity_record = crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
        .with_borrow(|s| s.get_record(graph_id, sanity_label_id))
        .expect("sanity record exists");
    assert!(
        matches!(
            sanity_record,
            EdgeInlinePropertySchemaRecord::InlineStruct {
                property_id,
                field_specs: _,
            } if property_id == sanity_property_id
        ),
        "sanity record must carry the top-level inline property identity"
    );
    assert_eq!(
        sanity_record.profile(),
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile::opaque_bytes(16),
        "sanity profile must be the derived opaque RawBytes projection"
    );

    // Pre-measurement sanity: canonical logical fields and derived opaque profile match the
    // intended fixed-size struct contract (16 bytes: 4 + 4 + 8).
    assert_eq!(layout.total_byte_width(), 16);
    assert_eq!(layout.fields().len(), 3);
    assert_eq!(
        layout.profile(),
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile::opaque_bytes(16)
    );

    // SCOPE NOTE: `set_inline_struct_schema` takes ownership of the layout, so the measured
    // closure clones the seed layout on every iteration. The reported cost therefore includes
    // both the canonical layout clone and the stable-record write; it is not a pure write-only
    // measurement. The clone is required by the current API and is representative of the real
    // commit path.
    canbench_rs::bench_fn(move || {
        let _scope = canbench_rs::bench_scope("inline_struct_schema_commit");
        crate::facade::stable::ROUTER_EDGE_INLINE_PROPERTY_PROFILES
            .with_borrow_mut(|s| {
                s.set_inline_struct_schema(graph_id, label_id, property_id, layout.clone())
            })
            .expect("commit inline struct schema");
    })
}

// -----------------------------------------------------------------------------
// Plan 0105: seed Candid transport benchmark-only probes.
// Compare nested (per-item blob inside outer vector) versus typed (outer vector
// of SeedBindingsWire) encoding/decoding for the POSTED complete-row seed shape.
// -----------------------------------------------------------------------------

use candid::{Decode, Encode};
use gleaph_graph_kernel::plan_exec::{SeedBindingsWire, SeedRowWire, SeedVertexBinding};

/// POSTED-shaped fixture: one variable, one row, one vertex binding, no float bindings,
/// no required labels, complete_prefix_rows=true, distinct local vertex id per item.
fn posted_seeds(count: usize) -> Vec<SeedBindingsWire> {
    let mut out = Vec::with_capacity(count);
    for local_vertex_id in 0..count as u32 {
        out.push(SeedBindingsWire {
            entries: Vec::new(),
            rows: vec![SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "poster".to_string(),
                    local_vertex_id,
                    required_vertex_label_ids: Vec::new(),
                }],
                float64_bindings: Vec::new(),
            }],
            complete_prefix_rows: true,
        });
    }
    out
}

/// One logical operation with `count` complete seed rows. This is intentionally different from
/// `posted_seeds(count)`, which models `count` logical operations with one row each.
fn posted_seed_rows(count: usize) -> SeedBindingsWire {
    SeedBindingsWire {
        entries: Vec::new(),
        rows: (0..count)
            .map(|local_vertex_id| SeedRowWire {
                vertex_bindings: vec![SeedVertexBinding {
                    variable: "poster".to_string(),
                    local_vertex_id: local_vertex_id as u32,
                    required_vertex_label_ids: Vec::new(),
                }],
                float64_bindings: Vec::new(),
            })
            .collect(),
        complete_prefix_rows: true,
    }
}

fn encode_nested(seeds: &[SeedBindingsWire]) -> Vec<u8> {
    let blobs: Vec<Option<Vec<u8>>> = seeds
        .iter()
        .map(|s| Some(Encode!(s).expect("encode seed")))
        .collect();
    Encode!(&blobs).expect("encode nested outer")
}

fn decode_nested(bytes: &[u8]) -> Vec<SeedBindingsWire> {
    let blobs: Vec<Option<Vec<u8>>> =
        Decode!(bytes, Vec<Option<Vec<u8>>>).expect("decode nested outer");
    blobs
        .into_iter()
        .map(|b| Decode!(&b.unwrap(), SeedBindingsWire).expect("decode inner seed"))
        .collect()
}

fn encode_typed(seeds: &[SeedBindingsWire]) -> Vec<u8> {
    Encode!(&seeds.to_vec()).expect("encode typed outer")
}

fn decode_typed(bytes: &[u8]) -> Vec<SeedBindingsWire> {
    Decode!(bytes, Vec<SeedBindingsWire>).expect("decode typed outer")
}

#[cfg(test)]
mod seed_transport_tests {
    use super::*;

    #[test]
    fn round_trip_nested_matches_fixture() {
        for n in [1usize, 32, 512] {
            let seeds = posted_seeds(n);
            let bytes = encode_nested(&seeds);
            let decoded = decode_nested(&bytes);
            assert_eq!(decoded, seeds, "nested round-trip failed at N={n}");
        }
    }

    #[test]
    fn round_trip_typed_matches_fixture() {
        for n in [1usize, 32, 512] {
            let seeds = posted_seeds(n);
            let bytes = encode_typed(&seeds);
            let decoded = decode_typed(&bytes);
            assert_eq!(decoded, seeds, "typed round-trip failed at N={n}");
        }
    }

    #[test]
    fn encoded_typed_not_larger_than_nested() {
        for n in [1usize, 32, 512] {
            let seeds = posted_seeds(n);
            let nested = encode_nested(&seeds).len();
            let typed = encode_typed(&seeds).len();
            assert!(
                typed <= nested,
                "typed encoding larger than nested at N={n}: {typed} > {nested}"
            );
        }
    }

    #[test]
    fn round_trip_empty_domain_seed() {
        let empty = SeedBindingsWire {
            entries: Vec::new(),
            rows: Vec::new(),
            complete_prefix_rows: true,
        };
        let nested = encode_nested(std::slice::from_ref(&empty));
        let typed = encode_typed(std::slice::from_ref(&empty));
        assert_eq!(decode_nested(&nested), vec![empty.clone()]);
        assert_eq!(decode_typed(&typed), vec![empty.clone()]);
    }

    #[test]
    fn encoded_byte_sizes_for_record() {
        for n in [1usize, 32, 512] {
            let seeds = posted_seeds(n);
            let nested = encode_nested(&seeds).len();
            let typed = encode_typed(&seeds).len();
            println!("seed_transport N={n} nested_bytes={nested} typed_bytes={typed}");
        }
    }
}

#[bench(raw)]
fn bench_seed_encode_nested_1() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(1);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_nested_1");
        black_box(encode_nested(&seeds));
    })
}

#[bench(raw)]
fn bench_seed_encode_nested_32() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(32);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_nested_32");
        black_box(encode_nested(&seeds));
    })
}

#[bench(raw)]
fn bench_seed_encode_nested_512() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(512);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_nested_512");
        black_box(encode_nested(&seeds));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_1() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(1);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_1");
        black_box(encode_typed(&seeds));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_32() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(32);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_32");
        black_box(encode_typed(&seeds));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_512() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(512);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_512");
        black_box(encode_typed(&seeds));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_one_operation_128_rows() -> canbench_rs::BenchResult {
    let seed = posted_seed_rows(128);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_one_operation_128_rows");
        black_box(candid::Encode!(&seed).expect("encode seed"));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_one_operation_512_rows() -> canbench_rs::BenchResult {
    let seed = posted_seed_rows(512);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_one_operation_512_rows");
        black_box(candid::Encode!(&seed).expect("encode seed"));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_one_operation_1024_rows() -> canbench_rs::BenchResult {
    let seed = posted_seed_rows(1024);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_one_operation_1024_rows");
        black_box(candid::Encode!(&seed).expect("encode seed"));
    })
}

#[bench(raw)]
fn bench_seed_encode_typed_one_operation_2048_rows() -> canbench_rs::BenchResult {
    let seed = posted_seed_rows(2048);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_encode_typed_one_operation_2048_rows");
        black_box(candid::Encode!(&seed).expect("encode seed"));
    })
}

#[bench(raw)]
fn bench_seed_decode_nested_1() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(1);
    let bytes = encode_nested(&seeds);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_decode_nested_1");
        black_box(decode_nested(&bytes));
    })
}

#[bench(raw)]
fn bench_seed_decode_nested_32() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(32);
    let bytes = encode_nested(&seeds);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_decode_nested_32");
        black_box(decode_nested(&bytes));
    })
}

#[bench(raw)]
fn bench_seed_decode_nested_512() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(512);
    let bytes = encode_nested(&seeds);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_decode_nested_512");
        black_box(decode_nested(&bytes));
    })
}

#[bench(raw)]
fn bench_seed_decode_typed_1() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(1);
    let bytes = encode_typed(&seeds);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_decode_typed_1");
        black_box(decode_typed(&bytes));
    })
}

#[bench(raw)]
fn bench_seed_decode_typed_32() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(32);
    let bytes = encode_typed(&seeds);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_decode_typed_32");
        black_box(decode_typed(&bytes));
    })
}

#[bench(raw)]
fn bench_seed_decode_typed_512() -> canbench_rs::BenchResult {
    let seeds = posted_seeds(512);
    let bytes = encode_typed(&seeds);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("seed_decode_typed_512");
        black_box(decode_typed(&bytes));
    })
}

// -----------------------------------------------------------------------------
// Plan 0203: durable bulk-load receipt bounds.
//
// These probes exercise the Router-owned child receipt value, public status projection, bounded
// Finalize scan, and bounded receipt-GC delete/cursor write.  Every cardinality is imported from
// the owning stable/public modules so a changed SSOT bound changes both the workload and its
// validation path together.
// -----------------------------------------------------------------------------

use crate::facade::stable::bulk_load::{
    BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP, BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP,
    BulkLoadChunkEnvelopeV1, BulkLoadChunkProgressV1, BulkLoadChunkReceiptKey,
    BulkLoadChunkReceiptRecordV1, BulkLoadGraphReceiptV1, BulkLoadGraphRequestV1,
    MAX_BULK_LOAD_RECEIPTS_PER_PAGE,
};
use crate::facade::stable::label_stats::{
    BulkLoadCoordinatorV1, BulkLoadFinalizeStageV1, BulkLoadLifecycleV1, BulkLoadTargetV1,
    ClientMutationKey, RouterMutationRecord,
};
use crate::facade::stable::{
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS, ROUTER_MUTATION_BY_CLIENT_KEY, ROUTER_MUTATION_COUNTER,
};
use crate::facade::store::bulk_load::BulkLoadGcStepResult;
use crate::types::{
    AtomicInsertReceiptV1, AtomicInsertVertexV1, BulkLoadChunkV1, MAX_ATOMIC_INSERT_OPERATIONS,
};
use candid::Principal;
use gleaph_graph_kernel::plan_exec::{
    GraphOrderedVertexBatchReceiptV1, OrderedVertexBatchGraphItemV1,
    OrderedVertexBatchGraphRequestV1, ResolvedLabelTable, ResolvedPropertyTable,
};
use ic_stable_structures::Storable;

const BULK_BENCH_GRAPH_ID: GraphId = GraphId::from_raw(989_001);
const BULK_BENCH_PARENT_ID: u64 = 9_890_001;
const BULK_BENCH_CLIENT_KEY: &str = "bench-bulk-load";

fn bulk_bench_target() -> BulkLoadTargetV1 {
    BulkLoadTargetV1 {
        shard_id: ShardId::new(0),
        graph_canister: Principal::self_authenticating([98; 32]),
    }
}

fn bulk_bench_graph_request(
    graph_id: GraphId,
    target: &BulkLoadTargetV1,
    operation_count: usize,
) -> BulkLoadGraphRequestV1 {
    BulkLoadGraphRequestV1::Vertex(OrderedVertexBatchGraphRequestV1 {
        graph_id,
        target_shard_id: target.shard_id,
        target_graph_canister: target.graph_canister,
        resolved_labels: ResolvedLabelTable::default(),
        resolved_properties: ResolvedPropertyTable::default(),
        items: (0..operation_count)
            .map(|_| OrderedVertexBatchGraphItemV1 {
                resolved_vertex_labels: Vec::new(),
                resolved_initial_properties: Vec::new(),
            })
            .collect(),
    })
}

fn bulk_bench_chunk_envelope(operation_count: usize) -> BulkLoadChunkV1 {
    BulkLoadChunkV1::Vertices(
        (0..operation_count)
            .map(|_| AtomicInsertVertexV1 {
                vertex_labels: Vec::new(),
                initial_properties: Vec::new(),
            })
            .collect(),
    )
}

fn bulk_bench_receipt_row(
    graph_id: GraphId,
    target: &BulkLoadTargetV1,
    parent_mutation_id: u64,
    chunk_index: u32,
    operation_count: usize,
    progress: BulkLoadChunkProgressV1,
) -> (BulkLoadChunkReceiptKey, BulkLoadChunkReceiptRecordV1) {
    let graph_request = bulk_bench_graph_request(graph_id, target, operation_count);
    let graph_request_fingerprint = graph_request.fingerprint().expect("Graph fingerprint");
    let chunk = bulk_bench_chunk_envelope(operation_count);
    let chunk_fingerprint = BulkLoadChunkEnvelopeV1::from_chunk(&chunk)
        .fingerprint()
        .expect("Chunk fingerprint");
    let (graph_request, graph_request_fingerprint, graph_receipt, public_receipt, completed_at_ns) =
        match progress {
            BulkLoadChunkProgressV1::CanonicalPending => (
                Some(graph_request),
                Some(graph_request_fingerprint),
                None,
                None,
                None,
            ),
            BulkLoadChunkProgressV1::CanonicalCommitted
            | BulkLoadChunkProgressV1::ProjectionPending
            | BulkLoadChunkProgressV1::RetirementPending
            | BulkLoadChunkProgressV1::Completed => {
                let graph_receipt =
                    BulkLoadGraphReceiptV1::Vertex(GraphOrderedVertexBatchReceiptV1 {
                        logical_vertex_count: operation_count as u64,
                        emitted_delta_first_seq: None,
                        emitted_delta_last_seq: None,
                        hot_forward_vertices: Vec::new(),
                        allocated_vertex_ids: (0..operation_count as u32).collect(),
                    });
                let public_receipt = AtomicInsertReceiptV1 {
                    logical_operation_count: operation_count as u64,
                    logical_vertex_count: operation_count as u64,
                    logical_edge_count: 0,
                    allocated_vertex_ids: vec![vec![0; 8]; operation_count],
                };
                let completed_at_ns =
                    (progress == BulkLoadChunkProgressV1::Completed).then_some(1_u64);
                // Completed rows are compacted; non-terminal rows retain the Graph request.
                let request =
                    (progress != BulkLoadChunkProgressV1::Completed).then_some(graph_request);
                let fingerprint = (progress != BulkLoadChunkProgressV1::Completed)
                    .then_some(graph_request_fingerprint);
                (
                    request,
                    fingerprint,
                    Some(graph_receipt),
                    Some(public_receipt),
                    completed_at_ns,
                )
            }
        };
    let row = BulkLoadChunkReceiptRecordV1 {
        chunk_fingerprint,
        graph_request,
        graph_request_fingerprint,
        child_mutation_id: parent_mutation_id + chunk_index as u64 + 1,
        progress,
        public_receipt,
        graph_receipt,
        completed_at_ns,
    };
    row.validate().expect("valid bulk-load benchmark row");
    (
        BulkLoadChunkReceiptKey::new(parent_mutation_id, chunk_index),
        row,
    )
}

fn bulk_bench_key() -> ClientMutationKey {
    ClientMutationKey::new(
        Principal::anonymous(),
        BULK_BENCH_GRAPH_ID,
        BULK_BENCH_CLIENT_KEY.to_owned(),
    )
}

fn reset_bulk_bench_maps() {
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| map.clear_new());
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| map.clear_new());
}

fn seed_bulk_bench_parent(lifecycle: BulkLoadLifecycleV1, chunk_count: u32) {
    reset_bulk_bench_maps();
    let target = bulk_bench_target();
    let mut coordinator = BulkLoadCoordinatorV1::new(target.clone());
    coordinator.logical_operation_count = chunk_count as u64;
    coordinator.logical_vertex_count = chunk_count as u64;
    coordinator.next_chunk_index = chunk_count;
    coordinator.committed_chunk_count = chunk_count;
    coordinator.completed_chunk_count = chunk_count;
    coordinator.lifecycle = lifecycle;
    coordinator
        .validate()
        .expect("valid bulk-load benchmark parent");
    let mut parent = RouterMutationRecord::new_bulk_load(BULK_BENCH_PARENT_ID, 0, coordinator)
        .expect("bulk-load benchmark parent");
    if parent.is_terminal() {
        parent.mark_terminal_at_ns(0);
    }
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| {
        map.insert(bulk_bench_key(), parent);
    });
    ROUTER_BULK_LOAD_CHUNK_RECEIPTS.with_borrow_mut(|map| {
        for chunk_index in 0..chunk_count {
            let (key, row) = bulk_bench_receipt_row(
                BULK_BENCH_GRAPH_ID,
                &target,
                BULK_BENCH_PARENT_ID,
                chunk_index,
                1,
                BulkLoadChunkProgressV1::Completed,
            );
            map.insert(key, row);
        }
    });
}

#[bench(raw)]
fn bench_bulk_load_receipt_insert_max_operations() -> canbench_rs::BenchResult {
    reset_bulk_bench_maps();
    ROUTER_MUTATION_COUNTER.with_borrow_mut(|counter| counter.set(BULK_BENCH_PARENT_ID));
    let target = bulk_bench_target();
    let coordinator = BulkLoadCoordinatorV1::new(target.clone());
    let parent = RouterMutationRecord::new_bulk_load(BULK_BENCH_PARENT_ID, 0, coordinator)
        .expect("bulk-load benchmark parent");
    ROUTER_MUTATION_BY_CLIENT_KEY.with_borrow_mut(|map| {
        map.insert(bulk_bench_key(), parent);
    });
    let (_key, row) = bulk_bench_receipt_row(
        BULK_BENCH_GRAPH_ID,
        &target,
        BULK_BENCH_PARENT_ID,
        0,
        MAX_ATOMIC_INSERT_OPERATIONS,
        BulkLoadChunkProgressV1::CanonicalPending,
    );
    let graph = BULK_BENCH_GRAPH_ID;
    let caller = Principal::anonymous();
    let client_key = BULK_BENCH_CLIENT_KEY;
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bulk_load_receipt_insert_max_operations");
        let encoded = row.to_bytes();
        black_box(encoded);
        let child_id = crate::facade::store::RouterStore::new()
            .admit_bulk_load_child(
                black_box(caller),
                black_box(graph),
                black_box(client_key),
                black_box(BULK_BENCH_PARENT_ID),
                black_box(0),
                black_box(row.chunk_fingerprint),
                black_box(row.clone()),
            )
            .expect("bulk-load benchmark child admission");
        black_box(child_id);
    })
}

#[bench(raw)]
fn bench_bulk_load_status_page_max_public_projection() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bulk_load_status_page_max_public_projection");
        crate::types::validate_max_receipts(black_box(MAX_BULK_LOAD_RECEIPTS_PER_PAGE))
            .expect("maximum bulk-load status page fits the response bound");
    })
}

#[bench(raw)]
fn bench_bulk_load_finalize_scan_max_rows() -> canbench_rs::BenchResult {
    let row_count = BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP * 2;
    seed_bulk_bench_parent(
        BulkLoadLifecycleV1::FinalizePending {
            stage: BulkLoadFinalizeStageV1::VerifyReceipts,
            cursor: 0,
        },
        row_count,
    );
    let store = crate::facade::store::RouterStore::new();
    let page_count = row_count.div_ceil(BULK_LOAD_FINALIZE_SCAN_ROWS_PER_STEP);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bulk_load_finalize_scan_max_rows");
        for _ in 0..page_count {
            let coordinator = store
                .finalize_bulk_load_step(
                    black_box(Principal::anonymous()),
                    black_box(BULK_BENCH_GRAPH_ID),
                    black_box(BULK_BENCH_CLIENT_KEY),
                    black_box(0),
                )
                .expect("bulk-load finalize scan");
            black_box(coordinator);
        }
    })
}

#[bench(raw)]
fn bench_bulk_load_receipt_gc_max_delete() -> canbench_rs::BenchResult {
    let row_count = BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP * 2;
    seed_bulk_bench_parent(BulkLoadLifecycleV1::Completed, row_count);
    let store = crate::facade::store::RouterStore::new();
    let page_count = row_count.div_ceil(BULK_LOAD_RECEIPT_GC_ROWS_PER_STEP);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("bulk_load_receipt_gc_max_delete");
        for _ in 0..=page_count {
            let result: BulkLoadGcStepResult = store
                .bulk_load_receipt_gc_step(
                    black_box(Principal::anonymous()),
                    black_box(BULK_BENCH_GRAPH_ID),
                    black_box(BULK_BENCH_CLIENT_KEY),
                    black_box(crate::facade::store::CLIENT_MUTATION_KEY_TTL_NS + 1),
                )
                .expect("bulk-load receipt GC step");
            black_box(result);
        }
    })
}

// -----------------------------------------------------------------------------
// GAP-2026-07-17-001 Router batch chunk-decision benchmarks.
//
// `graph_batch_chunk_len_for_dispatches` decides each Router → Graph batch chunk: it runs the
// message-sizing adaptive probe (`adaptive_fitting_prefix` over the inter-canister policy) and
// then caps the count by the shared instruction budget
// (`GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION` at 500M against the 35B dynamic budget ⇒ at
// most 70 operations per chunk). These benches measure the **decision** cost only — the encode
// probes performed before a chunk is dispatched. The Router's per-chunk dispatch reserve
// (`ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM`, 4B) must cover one chunk's dispatch, response
// construction, and cross-canister call; the decision cost measured here must be trivial against
// that reserve. Payload shapes mirror the `build_execute_args` closure in
// `execute_prepared_mutation`: a shared plan blob and params blob per operation plus per-dispatch
// seed bindings.
// -----------------------------------------------------------------------------

use crate::gql::graph_batch_chunk_len_for_dispatches;
use gleaph_graph_kernel::plan_exec::{ExecutePlanArgs, GqlExecutionMode};

/// Mirrors the `build_execute_args` closure in `execute_prepared_mutation`: one operation per
/// dispatch carrying the shared plan blob and params blob plus per-dispatch bindings.
fn bench_execute_args(
    dispatch: &ShardDispatch,
    plan_blob: Vec<u8>,
    params_blob: Vec<u8>,
) -> ExecutePlanArgs {
    ExecutePlanArgs {
        target_shard_id: dispatch.shard_id,
        element_id_encoding_key: [7; 16],
        mutation_id: Some(1),
        plan_blob,
        params_blob,
        mode: GqlExecutionMode::Update,
        seed_bindings_blob: dispatch.seed_bindings_blob.clone(),
        resolved_labels: Some(ResolvedLabelTable::default()),
        resolved_properties: Some(ResolvedPropertyTable::default()),
        indexed_properties: None,
        unique_claims: None,
        constrained_properties: None,
        local_unique_claims: None,
        local_constrained_properties: None,
        indexed_embeddings: None,
        resolved_search_blob: dispatch.resolved_search_blob.clone(),
    }
}

fn bench_dispatches(count: usize, seed_binding_bytes: usize) -> Vec<ShardDispatch> {
    (0..count)
        .map(|i| ShardDispatch {
            shard_id: ShardId::new(i as u32),
            graph_canister: Principal::self_authenticating([9; 32]),
            seed_bindings_blob: (seed_binding_bytes > 0).then(|| vec![i as u8; seed_binding_bytes]),
            resolved_search_blob: None,
        })
        .collect()
}

fn bench_chunk_decision(
    dispatches: Vec<ShardDispatch>,
    plan_blob: Vec<u8>,
    params_blob: Vec<u8>,
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let build_execute_args = |dispatch: &ShardDispatch| {
        bench_execute_args(dispatch, plan_blob.clone(), params_blob.clone())
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = graph_batch_chunk_len_for_dispatches(
            black_box(&dispatches),
            &build_execute_args,
            black_box(None),
        )
        .expect("bench chunk decision");
        black_box(result);
    })
}

/// Nominal single-chunk decision: 70 dispatches (the instruction-cap chunk size) with small
/// uniform payloads that fit the sizing target in one probe pass.
#[bench(raw)]
fn bench_graph_batch_chunk_len_for_dispatches_small() -> canbench_rs::BenchResult {
    bench_chunk_decision(
        bench_dispatches(70, 0),
        vec![7; 1024],
        Vec::new(),
        "graph_batch_chunk_len_small",
    )
}

/// Adversarial decision: heterogeneous per-dispatch payload sizes (a small leading sample, then
/// large plan blobs and seed bindings) make the fixed-sample estimate overshoot the hard limit,
/// forcing the proportional-reduction loop over a multi-MiB candidate encode.
#[bench(raw)]
fn bench_graph_batch_chunk_len_for_dispatches_adversarial() -> canbench_rs::BenchResult {
    let mut dispatches = bench_dispatches(512, 0);
    // First `sample_entries` (96) stay small; the rest carry a large plan blob and seed binding
    // so the sample-derived estimate overshoots and the while loop re-probes large prefixes.
    for dispatch in dispatches.iter_mut().skip(96) {
        dispatch.seed_bindings_blob = Some(vec![7; 2048]);
    }
    let mut plan_blob = vec![7; 1024];
    plan_blob.resize(64 * 1024, 7);
    bench_chunk_decision(
        dispatches,
        plan_blob,
        Vec::new(),
        "graph_batch_chunk_len_adversarial",
    )
}
