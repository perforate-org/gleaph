//! Manual PocketIC probe for the reverse-mirror share of LARA stable-memory bytes (audit C6).
//!
//! This is intentionally ignored: it bulk-loads a representative graph on a single-shard
//! federation and prints the full `admin_stable_memory_stats` region table plus forward/reverse
//! LARA share computations. It records measurement evidence only; no production behavior is
//! asserted beyond structural stats invariants. The recorded numbers live in
//! `design/investigations/2026-08-24-graph-persistence-audit.md`.
//!
//! Run explicitly:
//!
//! ```sh
//! cargo test -p gleaph-pocket-ic-tests --test lara_reverse_mirror_share_probe -- --ignored --nocapture
//! ```

use candid::{Decode, Encode};
use gleaph_gql::Value;
use gleaph_graph_kernel::stable_layout::{GRAPH_STABLE_LAYOUT, ProductionCompat};
use gleaph_graph_kernel::stable_memory::StableMemoryStats;
use gleaph_pocket_ic_tests::{
    FederationEnv, GRAPH_NAME, bulk_load_as_admin, bulk_load_status_as_admin, ensure_edge_label,
    ensure_property, ensure_vertex_label, install_single_shard_federation,
};
use gleaph_router::types::{
    AtomicInsertPropertyV1, AtomicInsertVertexV1, BulkLoadChunkV1, BulkLoadCommand, BulkLoadEdgeV1,
    BulkLoadEndpointV1, BulkLoadPublicStateV1, BulkLoadResponse,
};

const VERTEX_COUNT: u64 = 8_192;
/// Every vertex gets four outgoing directed edges (out-degree 4, in-degree 4).
const EDGE_OFFSETS: [u64; 4] = [1, 7, 13, 29];
const EDGE_COUNT: u64 = VERTEX_COUNT * EDGE_OFFSETS.len() as u64;
const CHUNK_ROWS: u64 = 512;

fn start(graph: &str, key: &str) -> BulkLoadCommand {
    BulkLoadCommand::Start {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
    }
}

fn append(graph: &str, key: &str, chunk_index: u32, chunk: BulkLoadChunkV1) -> BulkLoadCommand {
    BulkLoadCommand::Append {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
        chunk_index,
        chunk,
    }
}

fn finalize(graph: &str, key: &str) -> BulkLoadCommand {
    BulkLoadCommand::Finalize {
        graph_name: Some(graph.to_owned()),
        client_bulk_key: key.to_owned(),
    }
}

fn vertex_chunk(base_vertex: u64, rows: u64) -> BulkLoadChunkV1 {
    BulkLoadChunkV1::Vertices(
        (0..rows)
            .map(|offset| {
                let i = base_vertex + offset;
                AtomicInsertVertexV1 {
                    vertex_labels: vec![if i % 2 == 0 {
                        "Person".to_owned()
                    } else {
                        "Concept".to_owned()
                    }],
                    initial_properties: vec![AtomicInsertPropertyV1 {
                        property_name: "weight".to_owned(),
                        value: Value::Int64(i as i64)
                            .to_binary_bytes()
                            .expect("encode weight Int64"),
                    }],
                }
            })
            .collect(),
    )
}

fn edge_chunk(encoded_ids: &[Vec<u8>], base_edge: u64, rows: u64) -> BulkLoadChunkV1 {
    BulkLoadChunkV1::Edges(
        (0..rows)
            .map(|offset| {
                let e = base_edge + offset;
                let source = e / EDGE_OFFSETS.len() as u64;
                let target = (source + EDGE_OFFSETS[(e % EDGE_OFFSETS.len() as u64) as usize])
                    % VERTEX_COUNT;
                BulkLoadEdgeV1 {
                    source: BulkLoadEndpointV1::Existing(encoded_ids[source as usize].clone()),
                    target: BulkLoadEndpointV1::Existing(encoded_ids[target as usize].clone()),
                    directed: true,
                    edge_label_name: Some("LINKS_TO".to_owned()),
                    inline_property: None,
                    initial_edge_properties: Vec::new(),
                }
            })
            .collect(),
    )
}

fn drive_job(env: &FederationEnv, key: &str, chunks: Vec<BulkLoadChunkV1>) {
    assert_eq!(
        bulk_load_as_admin(env, start(GRAPH_NAME, key)),
        Ok(BulkLoadResponse::Started {
            next_chunk_index: 0,
        })
    );
    for (chunk_index, chunk) in chunks.into_iter().enumerate() {
        let chunk_index = chunk_index as u32;
        match bulk_load_as_admin(env, append(GRAPH_NAME, key, chunk_index, chunk))
            .expect("chunk append")
        {
            BulkLoadResponse::Appended {
                chunk_index: got, ..
            } => {
                assert_eq!(got, chunk_index, "chunks must commit in order");
            }
            unexpected => panic!("unexpected Append response: {unexpected:?}"),
        }
    }
    assert_eq!(
        bulk_load_as_admin(env, finalize(GRAPH_NAME, key)),
        Ok(BulkLoadResponse::FinalizeAccepted {
            state: BulkLoadPublicStateV1::Completed,
        })
    );
}

/// Page the completed job's receipts and return every allocated encoded vertex ID in order.
fn collect_allocated_ids(env: &FederationEnv, key: &str, expected: u64) -> Vec<Vec<u8>> {
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let page = bulk_load_status_as_admin(env, GRAPH_NAME, key, cursor, 32)
            .expect("completed status page");
        ids.extend(
            page.receipts
                .iter()
                .flat_map(|entry| entry.receipt.allocated_vertex_ids.iter().cloned()),
        );
        match page.next_receipt_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(ids.len() as u64, expected, "allocated id count mismatch");
    ids
}

#[test]
#[ignore = "manual stable-memory measurement; run explicitly with --nocapture"]
fn reverse_mirror_share_region_stats_print() {
    let env = install_single_shard_federation();
    ensure_vertex_label(&env, "Person");
    ensure_vertex_label(&env, "Concept");
    ensure_edge_label(&env, "LINKS_TO");
    ensure_property(&env, "weight");

    // Deterministic representative workload: 8,192 labeled vertices with one Int64 property,
    // then 32,768 directed edges (4 per vertex) over Existing endpoints.
    let vertex_key = "lara-c6-reverse-mirror-vertices";
    let vertex_chunks: Vec<_> = (0..VERTEX_COUNT / CHUNK_ROWS)
        .map(|i| vertex_chunk(i * CHUNK_ROWS, CHUNK_ROWS))
        .collect();
    drive_job(&env, vertex_key, vertex_chunks);
    let encoded_ids = collect_allocated_ids(&env, vertex_key, VERTEX_COUNT);

    let edge_key = "lara-c6-reverse-mirror-edges";
    let edge_chunks: Vec<_> = (0..EDGE_COUNT / CHUNK_ROWS)
        .map(|i| edge_chunk(&encoded_ids, i * CHUNK_ROWS, CHUNK_ROWS))
        .collect();
    drive_job(&env, edge_key, edge_chunks);

    // The graph handler is guarded to the Router canister, so call as the Router principal.
    let bytes = env
        .pic
        .query_call(
            env.graph_source,
            env.router,
            "admin_stable_memory_stats",
            Encode!(&()).expect("encode unit args"),
        )
        .expect("call admin_stable_memory_stats");
    let stats = Decode!(&bytes, StableMemoryStats).expect("decode StableMemoryStats");

    // Structural invariants only; byte-share values are environment-dependent evidence.
    let non_reserved_regions = GRAPH_STABLE_LAYOUT
        .regions
        .iter()
        .filter(|region| !matches!(region.compat, ProductionCompat::Reserved))
        .count();
    assert_eq!(
        stats.regions.len(),
        non_reserved_regions,
        "stats table must cover exactly the non-reserved GRAPH_STABLE_LAYOUT regions"
    );
    let forward_count = stats
        .regions
        .iter()
        .filter(|region| region.memory_id <= 14)
        .count();
    let reverse_count = stats
        .regions
        .iter()
        .filter(|region| (15..=29).contains(&region.memory_id))
        .count();
    assert_eq!(
        forward_count, 15,
        "forward LARA bundle must have 15 regions"
    );
    assert_eq!(
        reverse_count, 15,
        "reverse LARA bundle must have 15 regions"
    );
    assert_eq!(
        stats.logical_total_pages,
        stats.regions.iter().map(|r| r.logical_pages).sum::<u64>(),
        "logical_total_pages must equal the sum of per-region logical_pages"
    );

    let mut regions = stats.regions.clone();
    regions.sort_by_key(|region| region.memory_id);
    println!(
        "lara_reverse_mirror_probe workload vertices={VERTEX_COUNT} edges={EDGE_COUNT} (directed, 4 per vertex)"
    );
    println!(
        "{:>3} {:<48} {:>14} {:>14} {:>12}",
        "id", "region", "logical_pages", "alloc_pages", "slack"
    );
    for region in &regions {
        println!(
            "{:>3} {:<48} {:>14} {:>14} {:>12}",
            region.memory_id,
            region.name,
            region.logical_pages,
            region.allocated_pages,
            region.slack_pages
        );
    }

    let sum_pages =
        |lo: u8,
         hi: u8,
         pages: fn(&gleaph_graph_kernel::stable_memory::StableMemoryRegionStats) -> u64| {
            regions
                .iter()
                .filter(|region| (lo..=hi).contains(&region.memory_id))
                .map(pages)
                .sum::<u64>()
        };
    let bundle_logical = sum_pages(0, 29, |r| r.logical_pages);
    let bundle_allocated = sum_pages(0, 29, |r| r.allocated_pages);
    let fwd_logical = sum_pages(0, 14, |r| r.logical_pages);
    let fwd_allocated = sum_pages(0, 14, |r| r.allocated_pages);
    let rev_logical = sum_pages(15, 29, |r| r.logical_pages);
    let rev_allocated = sum_pages(15, 29, |r| r.allocated_pages);
    let total_slack = regions.iter().map(|r| r.slack_pages).sum::<u64>();
    println!(
        "SUMMARY lara_bundle(0-29) logical_pages={bundle_logical} allocated_pages={bundle_allocated}"
    );
    println!("SUMMARY forward(0-14) logical_pages={fwd_logical} allocated_pages={fwd_allocated}");
    println!("SUMMARY reverse(15-29) logical_pages={rev_logical} allocated_pages={rev_allocated}");
    println!(
        "SUMMARY reverse_share allocated={:.4}% logical={:.4}%",
        rev_allocated as f64 / bundle_allocated as f64 * 100.0,
        rev_logical as f64 / bundle_logical as f64 * 100.0,
    );
    println!("SUMMARY total_slack_pages(all non-reserved regions)={total_slack}");
}
