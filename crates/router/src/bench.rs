//! Phase 8 stable-memory layout benchmarks (ADR 0007 §6).
//!
//! Run from `crates/router`: `canbench` (see `canbench.yml`).

use crate::facade::stable::memory;
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
    black_box(memory::init_indexed_property_set());
    black_box(memory::init_edge_payload_profiles());
    black_box(memory::init_gql_graph_catalog());
    black_box(memory::init_graph_type_name_catalog());
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
fn bench_inline_edge_scalar_ddl_parse() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_ddl_parse");
        let stmt = crate::edge_payload_ddl::try_parse(
            "CREATE EDGE LABEL ROAD { distance FLOAT32 INLINE }",
        )
        .expect("recognised")
        .expect("valid");
        black_box(stmt);
    })
}

#[bench(raw)]
fn bench_inline_edge_scalar_schema_lookup() -> canbench_rs::BenchResult {
    let _store = RouterStore::new();
    let graph_id = bench_inline_graph_id();
    let label_id = RouterStore::commit_intern_edge_label_name(graph_id, "ROAD").expect("label");
    let property_id =
        RouterStore::commit_intern_property_name(graph_id, "distance").expect("property");
    crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES
        .with_borrow_mut(|s| {
            s.set_inline_scalar_schema(
                graph_id,
                label_id,
                property_id,
                crate::facade::stable::edge_payload_profiles::InlineScalarType::F32,
            )
        })
        .expect("seed inline schema");

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("inline_scalar_schema_lookup");
        let profile = crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES
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
        crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow_mut(|s| {
                s.set_inline_scalar_schema(
                    graph_id,
                    label_id,
                    property_id,
                    crate::facade::stable::edge_payload_profiles::InlineScalarType::F32,
                )
            })
            .expect("commit inline schema");
    })
}

#[bench(raw)]
fn bench_inline_edge_struct_schema_commit() -> canbench_rs::BenchResult {
    use crate::facade::stable::edge_payload_profiles::{
        EdgePayloadSchemaRecord, InlineScalarType, InlineStructLayout,
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
    crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES
        .with_borrow_mut(|s| {
            s.set_inline_struct_schema(
                graph_id,
                sanity_label_id,
                sanity_property_id,
                layout.clone(),
            )
        })
        .expect("sanity commit inline struct schema");
    let sanity_record = crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES
        .with_borrow(|s| s.get_record(graph_id, sanity_label_id))
        .expect("sanity record exists");
    assert!(
        matches!(
            sanity_record,
            EdgePayloadSchemaRecord::InlineStruct {
                property_id,
                field_specs: _,
            } if property_id == sanity_property_id
        ),
        "sanity record must carry the top-level inline property identity"
    );
    assert_eq!(
        sanity_record.profile(),
        gleaph_graph_kernel::entry::EdgePayloadProfile::opaque_bytes(16),
        "sanity profile must be the derived opaque RawBytes projection"
    );

    // Pre-measurement sanity: canonical logical fields and derived opaque profile match the
    // intended fixed-size struct contract (16 bytes: 4 + 4 + 8).
    assert_eq!(layout.total_byte_width(), 16);
    assert_eq!(layout.fields().len(), 3);
    assert_eq!(
        layout.profile(),
        gleaph_graph_kernel::entry::EdgePayloadProfile::opaque_bytes(16)
    );

    // SCOPE NOTE: `set_inline_struct_schema` takes ownership of the layout, so the measured
    // closure clones the seed layout on every iteration. The reported cost therefore includes
    // both the canonical layout clone and the stable-record write; it is not a pure write-only
    // measurement. The clone is required by the current API and is representative of the real
    // commit path.
    canbench_rs::bench_fn(move || {
        let _scope = canbench_rs::bench_scope("inline_struct_schema_commit");
        crate::facade::stable::ROUTER_EDGE_PAYLOAD_PROFILES
            .with_borrow_mut(|s| {
                s.set_inline_struct_schema(graph_id, label_id, property_id, layout.clone())
            })
            .expect("commit inline struct schema");
    })
}
