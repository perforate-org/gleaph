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
    black_box(memory::init_edge_inline_value_profiles());
    black_box(memory::init_gql_graph_catalog());
    black_box(memory::init_graph_type_name_catalog());
    black_box(memory::init_constraint_name_catalog());
    black_box(memory::init_unique_constraints());
    black_box(memory::init_unique_reservations());
    black_box(memory::init_mutation_reservation_index());
    black_box(memory::init_unique_effect_pending());
    black_box(memory::init_embedding_name_catalog());
    black_box(memory::init_vector_indexes());
    black_box(memory::init_vector_dispatch_activation());
    black_box(memory::init_vector_maintenance_policies());
    // provisioning
    black_box(memory::init_provisioning_requests());
    black_box(memory::init_provisioning_by_graph());
    black_box(memory::init_provisioning_intent_locks());
    black_box(memory::init_provision_config());
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
    let plan_blob = vec![0x5a; 256 * 1024];
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("router_prepared_plan_growth_32x256k");
        for i in 0..32u32 {
            insert_prepared_plan(
                PreparedPlanKey::new(graph_id, format!("capacity-plan-{i:02}")),
                PreparedPlanRecord::from_v1(PreparedPlanRecordV1 {
                    plan_blob: black_box(plan_blob.clone()),
                    requires_write_path: false,
                }),
            );
        }
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
        let stmt = crate::edge_inline_value_ddl::try_parse(
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
                crate::facade::stable::edge_inline_value_profiles::InlineScalarType::F32,
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
                    crate::facade::stable::edge_inline_value_profiles::InlineScalarType::F32,
                )
            })
            .expect("commit inline schema");
    })
}

#[bench(raw)]
fn bench_inline_edge_struct_schema_commit() -> canbench_rs::BenchResult {
    use crate::facade::stable::edge_inline_value_profiles::{
        EdgeInlineValueSchemaRecord, InlineScalarType, InlineStructLayout,
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
            EdgeInlineValueSchemaRecord::InlineStruct {
                property_id,
                field_specs: _,
            } if property_id == sanity_property_id
        ),
        "sanity record must carry the top-level inline property identity"
    );
    assert_eq!(
        sanity_record.profile(),
        gleaph_graph_kernel::entry::EdgeInlineValueProfile::opaque_bytes(16),
        "sanity profile must be the derived opaque RawBytes projection"
    );

    // Pre-measurement sanity: canonical logical fields and derived opaque profile match the
    // intended fixed-size struct contract (16 bytes: 4 + 4 + 8).
    assert_eq!(layout.total_byte_width(), 16);
    assert_eq!(layout.fields().len(), 3);
    assert_eq!(
        layout.profile(),
        gleaph_graph_kernel::entry::EdgeInlineValueProfile::opaque_bytes(16)
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
