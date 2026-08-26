//! Two-tier precision code tier coverage (Slice 6 / ADR 0078). Every test drives the public
//! facade (rebuild start with `code_tier = Some(true)` → steps → publish → search) and compares
//! against a brute-force exact reference computed in the test itself, so a broken encoder,
//! estimator, shortlist, or rerank cannot pass silently.

use super::*;
use gleaph_graph_kernel::vector_index::VectorSearchResult;

use crate::code_tier::QueryCode;
use crate::facade::stable::PAGE_STORE;
use crate::facade::stable::page_store::PageScratch;

const SAMPLE_LIMIT: u32 = 100;

fn start_tier(fine_nlist: Option<u32>) {
    admin_start_vector_rebuild_with_fine(
        router(),
        INDEX_ID,
        2,
        SAMPLE_LIMIT,
        fine_nlist,
        Some(true),
    )
    .expect("tier-on rebuild starts");
}

fn published_def() -> crate::records::VectorIndexDef {
    definition_store::get(INDEX_ID)
        .expect("definition readable")
        .expect("definition present")
}

/// Deterministic LCG stream for reproducible fixtures.
struct Lcg(u64);
impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1u32 << 24) as f32 - 0.5
    }
}

/// Seeds `rows` vectors around `clusters` well-separated centers (dims = [`DIMS`] = 4) and
/// returns the seeded values keyed by vertex id for brute-force references.
fn seed_clustered(rows: usize, clusters: usize) -> Vec<(u32, [f32; 4])> {
    let mut lcg = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        let c = i % clusters;
        let mut v = [0.0f32; 4];
        for (d, slot) in v.iter_mut().enumerate() {
            *slot = 12.0 * c as f32 + ((c * 7 + d * 3) as f32 * 0.21).sin() + 0.3 * lcg.next_f32();
        }
        let vertex = (i + 1) as u32;
        vector_upsert(
            shard_canister(),
            &upsert_vec_from(vertex, 1, &v, VectorMetric::L2Squared),
        )
        .expect("seed upsert");
        out.push((vertex, v));
    }
    out
}

/// Brute-force exact top-k over the seeded rows: `(distance asc, vertex asc)` — the same order
/// and tie-break the scan contract advertises.
fn brute_force(data: &[(u32, [f32; 4])], query: &[f32], k: usize) -> Vec<u32> {
    let mut scored: Vec<(f32, u32)> = data
        .iter()
        .map(|(vertex, v)| {
            let d: f32 = query
                .iter()
                .zip(v.iter())
                .map(|(a, b)| (a - b) * (a - b))
                .sum();
            (d, *vertex)
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(k).map(|(_, v)| v).collect()
}

fn hit_vertices(result: VectorSearchResult) -> Vec<u32> {
    result
        .hits
        .iter()
        .map(|h| match h.subject {
            VectorSubject::Vertex { vertex_id, .. } => vertex_id,
        })
        .collect()
}

fn tier_request(query_bytes: Vec<u8>, top_k: u32) -> VectorSearchRequest {
    VectorSearchRequest {
        index_id: INDEX_ID,
        query: query_bytes,
        encoding: VectorEncoding::F32,
        dims: DIMS,
        metric: VectorMetric::L2Squared,
        top_k,
        candidate_subjects: None,
    }
}

fn head_live_len() -> u64 {
    VECTOR_PARTITION_HEADS.with_borrow(|h| {
        h.get(&crate::records::PartitionKey::new(
            INDEX_ID,
            published_def().active_index_version,
            0,
        ))
        .expect("head get")
        .map(|record| match record {
            crate::records::PartitionHeadRecord::Head(head) => head.live_len,
            other => panic!("partition heads: unexpected record kind: {other:?}"),
        })
        .unwrap_or(0)
    })
}

/// Contract ③ (envelope case): when the live row count is within the shortlist capacity, the
/// tier-on search result equals the brute-force exact top-k exactly — same subjects in the same
/// order. Also proves the write path physically stored code segments (every page of the published
/// generation carries a populated code table).
#[test]
fn tier_on_search_equals_exact_ground_truth_within_envelope() {
    fresh_store();
    // 64 rows ≤ C = clamp(8·8, 128..=1024) = 128 → the whole index fits one shortlist.
    let data = seed_clustered(64, 4);
    start_tier(None);
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    assert!(
        published_def().has_code_tier(),
        "published def flips the tier on"
    );
    drive_cleanup(INDEX_ID);

    // Every page of the active generation carries a non-degenerate code table.
    PAGE_STORE.with_borrow(|store| {
        let version = published_def().active_index_version;
        let mut scratch = PageScratch::new();
        store.visit_partition_pages_grouped(INDEX_ID, version, 0, &mut scratch, |_, scratch| {
            assert!(scratch.has_code_table(), "page must carry a code table");
            for slot in 0..scratch.row_count() {
                if scratch.live_row_info(slot).is_some() {
                    assert!(
                        scratch.code_slice(slot).iter().any(|b| *b != 0),
                        "live row {slot} has a non-zero code segment"
                    );
                }
            }
        });
    });

    let expected = brute_force(&data, &[6.1, 6.2, 6.3, 6.4], 8);
    let got = hit_vertices(
        vector_search(&tier_request(vec_bytes_from(&[6.1, 6.2, 6.3, 6.4]), 8))
            .expect("tier search"),
    );
    assert_eq!(got.len(), 8);
    assert_eq!(
        got, expected,
        "shortlist covers every live row, so Stage B rerank must reproduce exact top-k"
    );
}

/// Contract ③ (recall measurement): beyond the envelope (`C < live rows`), the first-stage
/// estimate ranks real signal — recall@10 against brute force stays far above what a random
/// shortlist of size C would achieve (~C/N ≈ 0.43 here). The measured value is printed.
#[test]
fn tier_on_recall_at_k_beyond_envelope_is_reported() {
    fresh_store();
    // 300 rows in one partition: a single page holds all rows, C = 128 < 300.
    let data = seed_clustered(300, 6);
    start_tier(None);
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);

    let q = [6.05f32, 6.15, 6.25, 6.35];
    let expected = brute_force(&data, &q, 10);
    let hits: std::collections::HashSet<u32> = hit_vertices(
        vector_search(&search_metric_from(&q, 10, VectorMetric::L2Squared)).expect("tier search"),
    )
    .into_iter()
    .collect();
    let recall = expected.iter().filter(|v| hits.contains(v)).count() as f32 / 10.0;
    println!("recall@10 beyond envelope (C=128, N=300): {recall:.3}");
    assert!(
        recall >= 0.7,
        "recall@10 {recall} collapsed toward random-shortlist level (~{:.2})",
        128.0 / 300.0
    );
}

/// Contract ④: raising the shortlist capacity never worsens the candidate envelope — recall of
/// the exact top-5 is monotone non-decreasing in `C`, and `C ≥ N` degenerates to the exact scan.
#[test]
fn tier_on_shortlist_capacity_monotonically_improves_recall() {
    fresh_store();
    let data = seed_clustered(300, 6);
    start_tier(None);
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);

    let def = published_def();
    let q = [18.1f32, 18.4, 18.7, 19.0];
    let expected: std::collections::HashSet<u32> = brute_force(&data, &q, 5).into_iter().collect();
    // The prepared rotated query mirrors `search_impl`'s per-search preparation (L2 keeps the raw
    // query).
    let qc = QueryCode::prepare(&def, &q);
    let recall_for = |cap: usize| -> f32 {
        let result = super::super::search::scan_partitions_code_tier_with_cap(
            INDEX_ID,
            def.active_index_version,
            0..def.leaf_count(),
            &q,
            def.metric,
            def.encoding,
            0.0,
            &[],
            1.0,
            5,
            &qc,
            cap,
        );
        let hits = hit_vertices(result);
        hits.iter().filter(|v| expected.contains(v)).count() as f32 / 5.0
    };
    let r_small = recall_for(16);
    let r_mid = recall_for(64);
    let r_full = recall_for(4096);
    println!(
        "recall@5 by shortlist cap: C=16 → {r_small:.2}, C=64 → {r_mid:.2}, C=4096 → {r_full:.2}"
    );
    assert!(
        r_full >= r_mid && r_mid >= r_small,
        "recall must be monotone non-decreasing in C"
    );
    assert_eq!(r_full, 1.0, "C ≥ N degenerates to the exact scan");
}

/// Contract ⑤: both hierarchy shapes accept `code_tier = Some(true)`; each publishes a tier-on
/// generation whose search reproduces the exact ground truth inside the envelope.
#[test]
fn tier_on_combines_with_flat_and_two_level_shapes() {
    for fine in [None, Some(2u32)] {
        fresh_store();
        let data = seed_clustered(24, 3);
        start_tier(fine);
        assert_eq!(
            drive_steps(INDEX_ID).phase,
            VectorRebuildPhase::ReadyToPublish
        );
        admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
        let def = published_def();
        assert!(def.has_code_tier());
        assert_eq!(
            def.is_two_level(),
            fine.is_some(),
            "published hierarchy matches the request"
        );
        // ε₂ = INF walks every leaf, so the tier path must reproduce the exact top-k over the
        // whole index inside the envelope.
        let live: u64 = (0..def.leaf_count())
            .map(|p| {
                VECTOR_PARTITION_HEADS.with_borrow(|h| {
                    h.get(&crate::records::PartitionKey::new(
                        INDEX_ID,
                        def.active_index_version,
                        p,
                    ))
                    .expect("head get")
                    .map(|record| match record {
                        crate::records::PartitionHeadRecord::Head(head) => head.live_len,
                        other => panic!("partition heads: unexpected record kind: {other:?}"),
                    })
                    .unwrap_or(0)
                })
            })
            .sum();
        assert_eq!(live, 24, "all seeded rows stay live after publish+cleanup");
        let got = hit_vertices(
            vector_search_tuned(
                &search_metric_from(&[6.0, 6.0, 6.0, 6.0], 6, VectorMetric::L2Squared),
                tuned(f32::INFINITY),
            )
            .expect("tier search"),
        );
        assert_eq!(
            got,
            brute_force(&data, &[6.0, 6.0, 6.0, 6.0], 6),
            "shape × tier combination stays exact within the envelope"
        );
    }
}

/// Contract ⑥: the same-stamp idempotency comparison keeps looking at the **original** payload
/// only — a byte-identical replay on a tier-on generation stays a no-op and a different payload
/// at the same stamp still conflicts, regardless of the derived code segment.
#[test]
fn tier_on_idempotent_replay_compares_original_payload_only() {
    fresh_store();
    seed_clustered(8, 2);
    start_tier(None);
    assert_eq!(
        drive_steps(INDEX_ID).phase,
        VectorRebuildPhase::ReadyToPublish
    );
    admin_publish_vector_rebuild(router(), INDEX_ID).expect("publish");
    drive_cleanup(INDEX_ID);

    let op = upsert_vec_from(77, 50, &[1.0, 2.0, 3.0, 4.0], VectorMetric::L2Squared);
    vector_upsert(shard_canister(), &op).expect("first insert");
    let before = head_live_len();
    vector_upsert(shard_canister(), &op).expect("identical replay");
    assert_eq!(
        head_live_len(),
        before,
        "identical replay appends no row even though codes differ per append call"
    );
    // Same stamp, different payload → conflict (the comparison saw the original payload).
    let conflict = upsert_vec_from(77, 50, &[9.0, 2.0, 3.0, 4.0], VectorMetric::L2Squared);
    assert_eq!(
        vector_upsert(shard_canister(), &conflict),
        Err(VectorCanisterError::MutationStampConflict)
    );
    assert_eq!(
        head_live_len(),
        before,
        "conflict leaves the stored row alone"
    );
}
