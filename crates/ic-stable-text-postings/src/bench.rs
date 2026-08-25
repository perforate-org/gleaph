//! Text Index PoC measurement suite (plan 0293 Q1–Q4).
//!
//! Run from `crates/ic-stable-text-postings`: `canbench <pattern>` (see `canbench.yml`).
//! Every measured closure is self-contained: reader construction from black-boxed encoded
//! buffers is included because it is bounded by header/descriptor size and dwarfed by the
//! payload work on list A; fixture construction, correctness verification, and oracle
//! comparisons all happen outside `bench_fn` so a fast wrong path cannot be measurable.
//!
//! Determinism: the fixture is built once behind a [`OnceLock`](std::sync::OnceLock) from
//! a fixed-seed corpus; probe sets and target patterns come from a fixed xorshift stream.
//! No clocks, no hash iteration. A second, bigram-expanded M=2000 fixture (see
//! [`crate::expanded`]) backs the D1 closing benches; it follows the same pattern.

use std::hint::black_box;
use std::sync::OnceLock;

use canbench_rs::bench;

use crate::blockmax::{BlockMaxTable, LOGICAL_BLOCK_SIZE, logical_block_count};
use crate::enc::{
    AnyPostingReader, EfReader, ForReader, FreqVarintReader, PefReader, PostingReader,
    VarintReader, encode_elias_fano, encode_frame_of_reference, encode_freq_varint,
    encode_partitioned_ef, encode_varint,
};
use crate::expanded::{ExpandedCorpus, InvertedList, expanded_fixture, invert};
use crate::merge::{MergeCursor, MergeState};
use crate::topk::{Hit, QueryList, topk_disjunctive};

/// Fixed corpus seed behind every derived fixture.
const CORPUS_SEED: u64 = 2026_0823;
const CORPUS_DOCS: u32 = 300_000;
const CORPUS_AVG_LEN: u32 = 24;
const CORPUS_VOCAB: u32 = 2048;
const CORPUS_ZIPF_S: f64 = 1.0;

/// Vocabulary ranks whose document frequencies form the four benchmark lists:
/// A ≈ dense (>= 64k ids), B/C medium Zipf tails (~8–20k), D small (< 1k).
const RANK_A: usize = 0;
const RANK_B: usize = 53;
const RANK_C: usize = 83;
const RANK_D: usize = 1000;
const RANKS: [usize; 4] = [RANK_A, RANK_B, RANK_C, RANK_D];

/// Scoring weights the top-k fixture assigns to lists B, C, D (caller-owned scoring math).
const WEIGHT_B: u32 = 3;
const WEIGHT_C: u32 = 2;
const WEIGHT_D: u32 = 1;
const TOP_K: usize = 10;

const MERGE_OUT_BUDGET: usize = 8192;
const ADVANCE_CALLS: usize = 1024;
const DICT_ENTRIES: u64 = 16_384;
const DICT_HITS: usize = 256;
const DICT_MISSES: usize = 256;

/// Fixed-point scale for the fixture's synthetic per-posting score parts (see
/// [`scored_part_of`]); caller-owned scoring math lives above the crate — this exists only
/// so the scored bench has plausible integer parts to sum.
const PART_SCALE: u64 = 4096;

/// Damping constant of the fixture's synthetic scored-part model below.
const TF_DAMP: u32 = 8;

/// Fixture-side fixed-point stand-in for tf-damped scoring, redefined in plan 0295 slice
/// 11b when query-side score inputs became a pure tf→part lookup table (the driver now
/// reads tfs straight off the codec, so per-document lengths are no longer expressible):
///
/// ```text
/// scored_part(tf) = PART_SCALE * tf / (tf + TF_DAMP)
/// ```
///
/// Monotone in the stored term frequency, computed exactly in u64 and stored as u32 —
/// no floats anywhere in crate code. The scored bench sums these parts verbatim through
/// its lookup table; the crate itself owns no scoring formula.
fn scored_part_of(tf: u32) -> u32 {
    (PART_SCALE * u64::from(tf) / (u64::from(tf) + u64::from(TF_DAMP))) as u32
}

/// All-zero part table shared by the constant-weight benches (contribution = weight,
/// since tf-less codecs report one occurrence per posting and the table maps it to 0).
const ZERO_PARTS: crate::topk::TfPartTable = [0u8 as u32; 256];

/// xorshift64 for fixed probe/target patterns (fixture-side only).
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

#[derive(Default)]
struct EncodedLists {
    a_varint: Vec<u8>,
    a_for: Vec<u8>,
    a_ef: Vec<u8>,
    a_pef: Vec<u8>,
    b_for: Vec<u8>,
    c_for: Vec<u8>,
    d_for: Vec<u8>,
    b_varint: Vec<u8>,
    c_varint: Vec<u8>,
    d_varint: Vec<u8>,
    b_freq: Vec<u8>,
    c_freq: Vec<u8>,
    d_freq: Vec<u8>,
    c_pef: Vec<u8>,
    d_ef: Vec<u8>,
}

struct Fixture {
    plain_a: Vec<u32>,
    enc: EncodedLists,
    /// Weight-scaled block-max bounds in DOCID space: entry b caps any hit whose docid
    /// falls in docid-block b (0 where the list has no postings there).
    scaled_b: Vec<u32>,
    scaled_c: Vec<u32>,
    scaled_d: Vec<u32>,
    /// Caller-built tf→part table for the scored bench (fixture-side
    /// [`scored_part_of`] output), plus its per-list block-max tables.
    scored_lut: Box<crate::topk::TfPartTable>,
    scaled_scored_b: Vec<u32>,
    scaled_scored_c: Vec<u32>,
    scaled_scored_d: Vec<u32>,
    /// Sorted deduplicated union of the four lists (merge oracle).
    merge_oracle: Vec<u32>,
    /// Brute-force top-10 truth for the {B, C, D} disjunctive query under constant
    /// weights and under the scored part model.
    topk_truth: Vec<Hit>,
    parts_truth: Vec<Hit>,
    /// Fixed ascending advance-target pattern mixing before-first / mid-gap / exact-hit /
    /// past-end classes (see the advance bench doc comments).
    advance_targets: Vec<u32>,
    /// Interleaved hit/miss dictionary probes (256 hits then 256 misses).
    dict_probes: Vec<u64>,
}

static FIXTURE: OnceLock<Fixture> = OnceLock::new();

fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(build_fixture)
}

fn build_fixture() -> Fixture {
    let corpus = crate::corpus::generate(crate::corpus::CorpusConfig {
        seed: CORPUS_SEED,
        docs: CORPUS_DOCS,
        avg_len: CORPUS_AVG_LEN,
        vocab_size: CORPUS_VOCAB,
        zipf_s: CORPUS_ZIPF_S,
    });
    let mut lists: [Vec<u32>; 4] = Default::default();
    let mut tfs: [Vec<u32>; 4] = Default::default();
    for (docid, doc) in corpus.docs.iter().enumerate() {
        let mut counts = [0u32; 4];
        for token in doc {
            for (slot, rank) in RANKS.iter().enumerate() {
                if *token as usize == *rank {
                    counts[slot] += 1;
                }
            }
        }
        for slot in 0..4 {
            if counts[slot] > 0 {
                lists[slot].push(docid as u32);
                tfs[slot].push(counts[slot]);
            }
        }
    }
    drop(corpus);
    let [plain_a, plain_b, plain_c, plain_d] = lists;
    let [tf_a, tf_b, tf_c, tf_d] = tfs;
    drop(tf_a);

    // Document-frequency spread guards: the benchmark contracts depend on these bands.
    assert!(
        plain_a.len() >= 64_000,
        "list A must be dense, got {}",
        plain_a.len()
    );
    assert!(
        (8_000..=20_000).contains(&plain_b.len()),
        "list B out of band: {}",
        plain_b.len()
    );
    assert!(
        (8_000..=20_000).contains(&plain_c.len()),
        "list C out of band: {}",
        plain_c.len()
    );
    assert!(
        plain_d.len() <= 1_000,
        "list D must stay small, got {}",
        plain_d.len()
    );

    // Weight-scaled block-max bounds in DOCID space: entry b = weight x max tf among
    // list docs whose docid falls in docid-block b (0 when none), so the driver can
    // upper-bound any candidate directly by its docid block. The scored variant uses the
    // same layout over part values (weight 1).
    fn scale_docid_space(list: &[u32], values: &[u32], weight: u32) -> Vec<u32> {
        let mut bounds = vec![0u32; (CORPUS_DOCS / LOGICAL_BLOCK_SIZE + 1) as usize];
        for (pos, &docid) in list.iter().enumerate() {
            let entry = &mut bounds[(docid / LOGICAL_BLOCK_SIZE) as usize];
            *entry = (*entry).max(values[pos] * weight);
        }
        bounds
    }
    let scaled_b = scale_docid_space(&plain_b, &tf_b, WEIGHT_B);
    let scaled_c = scale_docid_space(&plain_c, &tf_c, WEIGHT_C);
    let scaled_d = scale_docid_space(&plain_d, &tf_d, WEIGHT_D);

    // Scored arm: the tf→part table and its per-list block-max tables (weight 1).
    let scored_lut: Box<crate::topk::TfPartTable> =
        Box::new(std::array::from_fn(|tf| scored_part_of(tf as u32)));
    let scored_of: Vec<Vec<u32>> = [(&tf_b), (&tf_c), (&tf_d)]
        .iter()
        .map(|tfs| {
            tfs.iter()
                .map(|&tf| scored_lut[tf.min(255) as usize])
                .collect()
        })
        .collect();
    let scaled_scored_b = scale_docid_space(&plain_b, &scored_of[0], 1);
    let scaled_scored_c = scale_docid_space(&plain_c, &scored_of[1], 1);
    let scaled_scored_d = scale_docid_space(&plain_d, &scored_of[2], 1);

    // Consistency gate: each scored block-max table recomputed a second way — direct
    // per-block docid windows via binary search — must agree with the incremental fill.
    fn assert_parts_tables_agree(list: &[u32], parts: &[u32], table: &[u32]) {
        let mut direct = vec![0u32; table.len()];
        for (blk, entry) in direct.iter_mut().enumerate() {
            let lo = blk as u32 * LOGICAL_BLOCK_SIZE;
            let hi = lo + LOGICAL_BLOCK_SIZE;
            let start = list.partition_point(|&docid| docid < lo);
            let end = list.partition_point(|&docid| docid < hi);
            *entry = parts[start..end].iter().copied().max().unwrap_or(0);
        }
        assert_eq!(direct, table, "parts block-max tables must agree two ways");
    }
    assert_parts_tables_agree(&plain_b, &scored_of[0], &scaled_scored_b);
    assert_parts_tables_agree(&plain_c, &scored_of[1], &scaled_scored_c);
    assert_parts_tables_agree(&plain_d, &scored_of[2], &scaled_scored_d);

    let enc = EncodedLists {
        a_varint: encode_varint(&plain_a),
        a_for: encode_frame_of_reference(&plain_a),
        a_ef: encode_elias_fano(&plain_a),
        a_pef: encode_partitioned_ef(&plain_a),
        b_for: encode_frame_of_reference(&plain_b),
        c_for: encode_frame_of_reference(&plain_c),
        d_for: encode_frame_of_reference(&plain_d),
        b_varint: encode_varint(&plain_b),
        c_varint: encode_varint(&plain_c),
        d_varint: encode_varint(&plain_d),
        b_freq: encode_freq_varint(&plain_b, &tf_b),
        c_freq: encode_freq_varint(&plain_c, &tf_c),
        d_freq: encode_freq_varint(&plain_d, &tf_d),
        c_pef: encode_partitioned_ef(&plain_c),
        d_ef: encode_elias_fano(&plain_d),
    };

    let mut merge_oracle: Vec<u32> = Vec::new();
    merge_oracle.extend_from_slice(&plain_a);
    merge_oracle.extend_from_slice(&plain_b);
    merge_oracle.extend_from_slice(&plain_c);
    merge_oracle.extend_from_slice(&plain_d);
    merge_oracle.sort_unstable();
    merge_oracle.dedup();

    let topk_truth = brute_force_topk(
        &plain_b,
        &plain_c,
        &plain_d,
        &[WEIGHT_B, WEIGHT_C, WEIGHT_D],
        TOP_K,
    );
    let parts_truth = brute_force_topk_parts(
        &plain_b,
        &plain_c,
        &plain_d,
        [&tf_b, &tf_c, &tf_d],
        &scored_lut,
        TOP_K,
    );
    let advance_targets = build_advance_targets(&plain_a);

    // Dictionary probes: monotone keys interleave gaps of 1..=3 via vocab parity.
    let dict_key = |j: u64| j * 2 + (j % u64::from(CORPUS_VOCAB)) % 2;
    let sorted_keys: Vec<u64> = (0..DICT_ENTRIES).map(dict_key).collect();
    let mut dict_probes = Vec::with_capacity(DICT_HITS + DICT_MISSES);
    let mut misses = Vec::with_capacity(DICT_MISSES);
    for i in 0..DICT_HITS {
        let j = (i as u64 * 64 + 13) % DICT_ENTRIES;
        dict_probes.push(sorted_keys[j as usize]);
        if i < DICT_MISSES {
            let mut miss = sorted_keys[j as usize] + 1;
            while sorted_keys.binary_search(&miss).is_ok() {
                miss += 1;
            }
            misses.push(miss);
        }
    }
    dict_probes.extend(misses);

    Fixture {
        plain_a,
        enc,
        scaled_b,
        scaled_c,
        scaled_d,
        scored_lut,
        scaled_scored_b,
        scaled_scored_c,
        scaled_scored_d,
        merge_oracle,
        topk_truth,
        parts_truth,
        advance_targets,
        dict_probes,
    }
}

/// Ascending target pattern: one before-first clamp up front, alternating exact-hit /
/// mid-gap draws through the body, one past-end overshoot at the tail. The forward-only
/// reader contract means the before-first class appears once per sweep; the recorded
/// amortized cost therefore mixes clamp, skip, and hit behavior deterministically.
fn build_advance_targets(list_a: &[u32]) -> Vec<u32> {
    let mut state = 0x5EED_CAFE_F00D_0001u64;
    let mut targets = Vec::with_capacity(ADVANCE_CALLS);
    targets.push(list_a[0].saturating_sub(1));
    while targets.len() + 1 < ADVANCE_CALLS {
        let r = next_rand(&mut state);
        let idx = (r % list_a.len() as u64) as usize;
        let gap_mid = r & (1 << 62) == 0;
        let value = if gap_mid && idx + 1 < list_a.len() && list_a[idx + 1] > list_a[idx] + 1 {
            list_a[idx] + (list_a[idx + 1] - list_a[idx]) / 2
        } else {
            list_a[idx]
        };
        targets.push(value);
    }
    targets.push(list_a[list_a.len() - 1].saturating_add(1));
    targets.sort_unstable();
    targets
}

/// Sweep oracle mirroring reader `advance` semantics over an ascending target pattern:
/// the cursor rests ON each hit (unconsumed), exhaustion is sticky.
fn sweep_oracle(docs: &[u32], targets: &[u32]) -> Vec<Option<u32>> {
    let mut idx = 0usize;
    let mut out = Vec::with_capacity(targets.len());
    for &target in targets {
        while idx < docs.len() && docs[idx] < target {
            idx += 1;
        }
        out.push(docs.get(idx).copied());
    }
    out
}

/// Brute-force disjunctive top-k oracle: pointer-merge the plain lists, then take the k
/// best hits by (score desc, docid asc).
fn brute_force_topk(b: &[u32], c: &[u32], d: &[u32], weights: &[u32; 3], k: usize) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    let (mut ib, mut ic, mut id) = (0usize, 0usize, 0usize);
    while ib < b.len() || ic < c.len() || id < d.len() {
        let nexts = [
            b.get(ib).copied().unwrap_or(u32::MAX),
            c.get(ic).copied().unwrap_or(u32::MAX),
            d.get(id).copied().unwrap_or(u32::MAX),
        ];
        let candidate = nexts[0].min(nexts[1]).min(nexts[2]);
        let mut score = 0;
        if nexts[0] == candidate {
            score += weights[0];
            ib += 1;
        }
        if nexts[1] == candidate {
            score += weights[1];
            ic += 1;
        }
        if nexts[2] == candidate {
            score += weights[2];
            id += 1;
        }
        match hits.binary_search_by(|h| h.docid.cmp(&candidate)) {
            Ok(_) => unreachable!("pointer merge yields distinct docids"),
            Err(pos) => hits.insert(
                pos,
                Hit {
                    score,
                    docid: candidate,
                },
            ),
        }
    }
    hits.sort_by(|x, y| y.score.cmp(&x.score).then(x.docid.cmp(&y.docid)));
    hits.truncate(k);
    hits
}

/// Brute-force disjunctive top-k oracle for the scored model: pointer-merge the plain
/// lists, summing each matching list's tf→part lookup entry at the candidate's stored tf,
/// then take the k best hits by (score desc, docid asc).
fn brute_force_topk_parts(
    b: &[u32],
    c: &[u32],
    d: &[u32],
    tfs: [&[u32]; 3],
    lut: &crate::topk::TfPartTable,
    k: usize,
) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    let (mut ib, mut ic, mut id) = (0usize, 0usize, 0usize);
    while ib < b.len() || ic < c.len() || id < d.len() {
        let nexts = [
            b.get(ib).copied().unwrap_or(u32::MAX),
            c.get(ic).copied().unwrap_or(u32::MAX),
            d.get(id).copied().unwrap_or(u32::MAX),
        ];
        let candidate = nexts[0].min(nexts[1]).min(nexts[2]);
        let mut score = 0;
        if nexts[0] == candidate {
            score += lut[tfs[0][ib].min(255) as usize];
            ib += 1;
        }
        if nexts[1] == candidate {
            score += lut[tfs[1][ic].min(255) as usize];
            ic += 1;
        }
        if nexts[2] == candidate {
            score += lut[tfs[2][id].min(255) as usize];
            id += 1;
        }
        match hits.binary_search_by(|h| h.docid.cmp(&candidate)) {
            Ok(_) => unreachable!("pointer merge yields distinct docids"),
            Err(pos) => hits.insert(
                pos,
                Hit {
                    score,
                    docid: candidate,
                },
            ),
        }
    }
    hits.sort_by(|x, y| y.score.cmp(&x.score).then(x.docid.cmp(&y.docid)));
    hits.truncate(k);
    hits
}

// -- one-time correctness verification (outside every measured closure) -----------------

fn decoded_sum(bytes: &[u8], kind: &str) -> u64 {
    let mut acc = 0u64;
    match kind {
        "varint" => {
            let mut r = VarintReader::new(bytes);
            while let Some(d) = r.next() {
                acc += u64::from(d);
            }
        }
        "for" => {
            let mut r = ForReader::new(bytes);
            while let Some(d) = r.next() {
                acc += u64::from(d);
            }
        }
        "ef" => {
            let mut r = EfReader::new(bytes);
            while let Some(d) = r.next() {
                acc += u64::from(d);
            }
        }
        _ => {
            let mut r = PefReader::new(bytes);
            while let Some(d) = r.next() {
                acc += u64::from(d);
            }
        }
    }
    acc
}

fn verify_walk_checksums() {
    let f = fixture();
    let oracle = sum_of(&f.plain_a);
    assert_eq!(decoded_sum(&f.enc.a_varint, "varint"), oracle);
    assert_eq!(decoded_sum(&f.enc.a_for, "for"), oracle);
    assert_eq!(decoded_sum(&f.enc.a_ef, "ef"), oracle);
    assert_eq!(decoded_sum(&f.enc.a_pef, "pef"), oracle);
}

fn sum_of(list: &[u32]) -> u64 {
    list.iter().map(|d| u64::from(*d)).sum()
}

fn fresh_merge_runs<'a>(f: &'a Fixture) -> Vec<AnyPostingReader<'a>> {
    vec![
        AnyPostingReader::Varint(VarintReader::new(&f.enc.a_varint)),
        AnyPostingReader::For(ForReader::new(&f.enc.b_for)),
        AnyPostingReader::Pef(PefReader::new(&f.enc.c_pef)),
        AnyPostingReader::Ef(EfReader::new(&f.enc.d_ef)),
    ]
}

fn empty_merge_cursor() -> MergeCursor {
    MergeCursor {
        positions: vec![0; 4],
        last_emitted: None,
    }
}

fn verify_merge() {
    let f = fixture();
    let mut state = MergeState::new(fresh_merge_runs(f));
    let mut full = Vec::new();
    while state.merge_step(usize::MAX, &mut full) > 0 {}
    assert_eq!(
        full, f.merge_oracle,
        "full merge must equal the plain oracle"
    );

    let mut state = MergeState::restore(fresh_merge_runs(f), &empty_merge_cursor());
    let mut head = Vec::new();
    assert_eq!(
        state.merge_step(MERGE_OUT_BUDGET, &mut head),
        MERGE_OUT_BUDGET,
        "the union exceeds the step budget"
    );
    assert_eq!(&head[..], &f.merge_oracle[..MERGE_OUT_BUDGET]);
}

/// Runs the DAAT/WAND driver over the three query lists opened as `readers` (B, C, D
/// order). Generic over the reader codec so the frame-encoded, varint, and scored
/// freq-varint arms share one driver implementation: identical workload, only the
/// encoding and the caller-supplied contribution/bound model differ.
fn run_topk_over<'a, R: PostingReader>(
    readers: [R; 3],
    weights: [u32; 3],
    luts: [&'a crate::topk::TfPartTable; 3],
    bounds: [&'a [u32]; 3],
) -> Vec<Hit> {
    let [reader_b, reader_c, reader_d] = readers;
    let mut lists = [
        QueryList::new(reader_b, weights[0], bounds[0], luts[0]),
        QueryList::new(reader_c, weights[1], bounds[1], luts[1]),
        QueryList::new(reader_d, weights[2], bounds[2], luts[2]),
    ];
    topk_disjunctive(&mut lists, TOP_K)
}

/// The constant-weight arms' contribution model: each matching list adds its fixed
/// benchmark weight.
const BM_WEIGHTS: [u32; 3] = [WEIGHT_B, WEIGHT_C, WEIGHT_D];

fn run_topk(f: &Fixture) -> Vec<Hit> {
    run_topk_over(
        [
            ForReader::new(&f.enc.b_for),
            ForReader::new(&f.enc.c_for),
            ForReader::new(&f.enc.d_for),
        ],
        BM_WEIGHTS,
        [&ZERO_PARTS, &ZERO_PARTS, &ZERO_PARTS],
        [&f.scaled_b[..], &f.scaled_c[..], &f.scaled_d[..]],
    )
}

fn run_topk_varint(f: &Fixture) -> Vec<Hit> {
    run_topk_over(
        [
            VarintReader::new(&f.enc.b_varint),
            VarintReader::new(&f.enc.c_varint),
            VarintReader::new(&f.enc.d_varint),
        ],
        BM_WEIGHTS,
        [&ZERO_PARTS, &ZERO_PARTS, &ZERO_PARTS],
        [&f.scaled_b[..], &f.scaled_c[..], &f.scaled_d[..]],
    )
}

/// Scored arm: tf-carrying freq-varint readers whose postings are scored purely through
/// the fixture's tf→part lookup table (zero constant weight; contributions are table
/// entries). Block bounds are the scored parts' own block-max tables so pruning bounds
/// exactly what the driver sums.
fn run_topk_bm25(f: &Fixture) -> Vec<Hit> {
    let lut = &*f.scored_lut;
    run_topk_over(
        [
            FreqVarintReader::new(&f.enc.b_freq),
            FreqVarintReader::new(&f.enc.c_freq),
            FreqVarintReader::new(&f.enc.d_freq),
        ],
        [0, 0, 0],
        [lut, lut, lut],
        [
            &f.scaled_scored_b[..],
            &f.scaled_scored_c[..],
            &f.scaled_scored_d[..],
        ],
    )
}

fn verify_topk() {
    let f = fixture();
    let got = run_topk(f);
    assert_eq!(got, f.topk_truth, "DAAT/WAND driver must match brute force");
}

fn verify_topk_varint() {
    let f = fixture();
    assert_eq!(
        run_topk_varint(f),
        f.topk_truth,
        "varint DAAT/WAND driver must match brute force"
    );
}

fn verify_topk_bm25() {
    let f = fixture();
    assert_eq!(
        run_topk_bm25(f),
        f.parts_truth,
        "scored freq-varint driver must match the part-model brute force"
    );
}

fn open_reader(kind: &str, bytes: &'static [u8]) -> AnyPostingReader<'static> {
    match kind {
        "varint" => AnyPostingReader::Varint(VarintReader::new(bytes)),
        "for" => AnyPostingReader::For(ForReader::new(bytes)),
        "ef" => AnyPostingReader::Ef(EfReader::new(bytes)),
        _ => AnyPostingReader::Pef(PefReader::new(bytes)),
    }
}

fn verify_advance_oracle() {
    let f = fixture();
    let wants = sweep_oracle(&f.plain_a, &f.advance_targets);
    for (name, bytes) in [
        ("varint", &f.enc.a_varint),
        ("for", &f.enc.a_for),
        ("ef", &f.enc.a_ef),
        ("pef", &f.enc.a_pef),
    ] {
        let mut reader = open_reader(name, bytes);
        for (i, &target) in f.advance_targets.iter().enumerate() {
            assert_eq!(
                reader.advance(target),
                wants[i],
                "{name}: advance({target}) diverged from the oracle"
            );
        }
    }
}

// -- benches -----------------------------------------------------------------------------

/// Q1 ranking evidence; no absolute threshold.
///
/// Full sequential decode of dense list A through the delta-varint codec. Setup verifies
/// the walked checksum against the plain-oracle sum before any measurement.
#[bench(raw)]
fn bench_postings_walk_varint() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_walk_checksums();
    let bytes = black_box(&f.enc.a_varint);
    canbench_rs::bench_fn(|| {
        let mut reader = VarintReader::new(bytes);
        let mut checksum = 0u64;
        while let Some(d) = reader.next() {
            checksum += u64::from(d);
        }
        black_box(checksum);
    })
}

/// Q1 ranking evidence; no absolute threshold.
///
/// Full sequential decode of dense list A through Framing-of-Reference blocks. Setup
/// verifies the walked checksum against the plain-oracle sum before measurement.
#[bench(raw)]
fn bench_postings_walk_for() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_walk_checksums();
    let bytes = black_box(&f.enc.a_for);
    canbench_rs::bench_fn(|| {
        let mut reader = ForReader::new(bytes);
        let mut checksum = 0u64;
        while let Some(d) = reader.next() {
            checksum += u64::from(d);
        }
        black_box(checksum);
    })
}

/// Q1 ranking evidence; no absolute threshold.
///
/// Full sequential decode of dense list A through plain Elias-Fano. Setup verifies the
/// walked checksum against the plain-oracle sum before measurement.
#[bench(raw)]
fn bench_postings_walk_ef() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_walk_checksums();
    let bytes = black_box(&f.enc.a_ef);
    canbench_rs::bench_fn(|| {
        let mut reader = EfReader::new(bytes);
        let mut checksum = 0u64;
        while let Some(d) = reader.next() {
            checksum += u64::from(d);
        }
        black_box(checksum);
    })
}

/// Q1 ranking evidence; no absolute threshold.
///
/// Full sequential decode of dense list A through minimal Partitioned Elias-Fano. Setup
/// verifies the walked checksum against the plain-oracle sum before measurement.
#[bench(raw)]
fn bench_postings_walk_pef() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_walk_checksums();
    let bytes = black_box(&f.enc.a_pef);
    canbench_rs::bench_fn(|| {
        let mut reader = PefReader::new(bytes);
        let mut checksum = 0u64;
        while let Some(d) = reader.next() {
            checksum += u64::from(d);
        }
        black_box(checksum);
    })
}

/// Q1 advance-cost ranking; amortized instr/call is the recorded metric.
///
/// 1024 `advance(target)` calls over dense list A with a fixed xorshift-derived ascending
/// pattern mixing before-first / mid-gap / exact-hit / past-end target classes. Each
/// measured invocation rebuilds the reader (header parse only) and replays the whole
/// pattern; setup verifies every call against a sweep oracle once.
#[bench(raw)]
fn bench_postings_advance_varint() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_advance_oracle();
    let bytes = black_box(&f.enc.a_varint);
    let targets = black_box(&f.advance_targets);
    canbench_rs::bench_fn(|| {
        let mut reader = VarintReader::new(bytes);
        let mut checksum = 0u64;
        for &target in targets {
            checksum += u64::from(reader.advance(target).unwrap_or(u32::MAX));
        }
        black_box(checksum);
    })
}

/// Q1 advance-cost ranking; amortized instr/call is the recorded metric.
///
/// 1024 `advance(target)` calls over dense list A through FOR blocks; setup verifies the
/// pattern against a sweep oracle once.
#[bench(raw)]
fn bench_postings_advance_for() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_advance_oracle();
    let bytes = black_box(&f.enc.a_for);
    let targets = black_box(&f.advance_targets);
    canbench_rs::bench_fn(|| {
        let mut reader = ForReader::new(bytes);
        let mut checksum = 0u64;
        for &target in targets {
            checksum += u64::from(reader.advance(target).unwrap_or(u32::MAX));
        }
        black_box(checksum);
    })
}

/// Q1 advance-cost ranking; amortized instr/call is the recorded metric.
///
/// 1024 `advance(target)` calls over dense list A through plain Elias-Fano; setup verifies
/// the pattern against a sweep oracle once.
#[bench(raw)]
fn bench_postings_advance_ef() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_advance_oracle();
    let bytes = black_box(&f.enc.a_ef);
    let targets = black_box(&f.advance_targets);
    canbench_rs::bench_fn(|| {
        let mut reader = EfReader::new(bytes);
        let mut checksum = 0u64;
        for &target in targets {
            checksum += u64::from(reader.advance(target).unwrap_or(u32::MAX));
        }
        black_box(checksum);
    })
}

/// Q1 advance-cost ranking; amortized instr/call is the recorded metric.
///
/// 1024 `advance(target)` calls over dense list A through Partitioned Elias-Fano; setup
/// verifies the pattern against a sweep oracle once.
#[bench(raw)]
fn bench_postings_advance_pef() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_advance_oracle();
    let bytes = black_box(&f.enc.a_pef);
    let targets = black_box(&f.advance_targets);
    canbench_rs::bench_fn(|| {
        let mut reader = PefReader::new(bytes);
        let mut checksum = 0u64;
        for &target in targets {
            checksum += u64::from(reader.advance(target).unwrap_or(u32::MAX));
        }
        black_box(checksum);
    })
}

/// Q3; step must fit the 40B-instruction message budget with >=10x margin; heap bytes
/// written are the proxy until the stable store exists (hard cap 2 GiB writes/message).
///
/// One resumable `merge_step(out_budget = 8192)` over four mixed-encoding runs
/// (A varint + B frame + C pef + D ef, overlapping docid ranges). The measured step
/// re-reads its cursor position first (fresh readers + restore), mirroring the
/// stable-store contract where every step reloads resumable state. Setup asserts the
/// fully merged union against the plain oracle and that one budget-sized step reproduces
/// the oracle prefix.
#[bench(raw)]
fn bench_merge_step_k4_b8192() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_merge();
    let cursor = black_box(empty_merge_cursor());
    let mut out: Vec<u32> = Vec::with_capacity(MERGE_OUT_BUDGET);
    canbench_rs::bench_fn(move || {
        let mut state = MergeState::restore(fresh_merge_runs(f), &cursor);
        out.clear();
        let emitted = state.merge_step(MERGE_OUT_BUDGET, &mut out);
        black_box((emitted, &out));
    })
}

/// Q2 baseline for the v0 B-tree dictionary decision.
///
/// 512 mixed hit/miss `get()`s (256 + 256, deterministic sampling) against a
/// `StableBTreeMap<u64, u64>` of 16_384 entries keyed by corpus-term-id-derived monotone
/// gaps, inserted once behind an entry-count guard. Setup verifies every probe outcome.
#[bench(raw)]
fn bench_dict_lookup_btree_512() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_dict();
    let probes = black_box(&f.dict_probes);
    canbench_rs::bench_fn(|| {
        let mut seen = 0u64;
        with_dictionary(|map| {
            for probe in probes {
                if map.get(probe).is_some() {
                    seen += 1;
                }
            }
        });
        black_box(seen);
    })
}

/// **Predeclared threshold:** <=50M instructions (working target ~1% of the 5B query-call
/// budget); exceeding it fails Q4 feasibility.
///
/// Disjunctive top-10 over query lists {B, C, D} using a minimal DAAT WAND-pivot driver
/// with block-max upper-bound skipping over frame-encoded readers plus block-max tables.
/// Setup asserts the driver's result against a brute-force oracle once before measuring.
#[bench(raw)]
fn bench_topk_bmw_m3_top10() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_topk();
    canbench_rs::bench_fn(move || {
        let hits = run_topk(f);
        black_box(hits);
    })
}

/// D1 closing evidence for the m3 headline gap (interpretation question it answers: how
/// much of FTS5's 34.84 M vs the custom arm's 74.23 M was our own encoding choice rather
/// than engine quality — Q1 ranked varint the fastest codec, yet the recorded topk ran on
/// frame-encoded readers).
///
/// Identical workload, driver, probe lists ({53, 83, 1000}), weights, and block-max
/// tables as `bench_topk_bmw_m3_top10`; only the three query-side readers switch to
/// varint encoding. The block-max tables live in DOCID space at 128-doc logical
/// alignment, which no codec alters, so pruning behavior is unchanged. Setup verifies the
/// varint driver against the same brute-force oracle once before measuring.
///
/// **Predeclared threshold:** informational only — compare against 74.23 M (frame-encoded)
/// and the FTS5 arm's 34.84 M for the D1 engine decision.
#[bench(raw)]
fn bench_topk_bmw_m3_top10_varint() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_topk_varint();
    canbench_rs::bench_fn(move || {
        let hits = run_topk_varint(f);
        black_box(hits);
    })
}

/// D1 follow-up: tf-carrying scored variant of the m3 top-10. Same probe lists
/// ({53, 83, 1000}), same ported allocation-free driver; the query-side readers use
/// [`FreqVarintReader`] over the slice-6 interleaved (delta-docid varint + u8 tf) layout,
/// and each posting contributes the fixture's tf→part lookup entry
/// (`scored_part(tf) = 4096 * tf / (tf + 8)` — see the fixture docs; plan 0295 slice 11b
/// restricted score inputs to pure tf lookups). The crate owns no scoring math: parts are
/// inputs summed verbatim at candidates, and the block-max bounds cap part sums per
/// docid-block.
///
/// Setup asserts the result against a brute-force scored-model oracle once before
/// measuring.
///
/// **Predeclared threshold:** informational only — compare against the constant-weight
/// `bench_topk_bmw_m3_top10_varint` number to price real scoring on top of traversal.
#[bench(raw)]
fn bench_topk_bm25_m3_top10() -> canbench_rs::BenchResult {
    let f = black_box(fixture());
    verify_topk_bm25();
    canbench_rs::bench_fn(move || {
        let hits = run_topk_bm25(f);
        black_box(hits);
    })
}

// -- D1 closing: full physical build over the expanded M=2000 fixture --------------------

/// B-tree dictionary shape used by the build-segment bench (unit_id → df), matching the
/// Q2 lookup fixture's key/value types.
type BuildDictMap = ic_stable_structures::BTreeMap<
    u64,
    u64,
    ic_stable_structures::memory_manager::VirtualMemory<ic_stable_structures::DefaultMemoryImpl>,
>;

/// Stable-memory region the measured closure writes its dictionary into. Kept virgin
/// until the closure so every allocated page shows up in the reported SMI.
const BUILD_DICT_MEMORY_ID: u8 = 32;

/// Separate region for the full-path correctness build: gate allocations land here and
/// can never pre-grow the measurement region or mask its SMI.
const VERIFY_DICT_MEMORY_ID: u8 = 33;

fn open_dict(memory_id: u8) -> BuildDictMap {
    let memory = ic_stable_structures::memory_manager::MemoryManager::init(
        ic_stable_structures::DefaultMemoryImpl::default(),
    )
    .get(ic_stable_structures::memory_manager::MemoryId::new(
        memory_id,
    ));
    ic_stable_structures::BTreeMap::new(memory)
}

/// Builds one block-max table from an inverted list: raw max tf per consecutive
/// 128-posting window — the same positional blocking as FOR's physical blocks, so the
/// table holds exactly `ceil(df / 128)` u32 entries ([`BlockMaxTable::new`] enforces the
/// alignment invariant) and its byte footprint matches the parity definition. Values are
/// the fixture-level synthetic stand-in for caller-owned score bounds (scoring lives
/// above this crate).
fn build_block_max_table(list: &InvertedList) -> BlockMaxTable {
    let mut values = vec![0u32; logical_block_count(list.docids.len() as u32) as usize];
    for (pos, &tf) in list.tfs.iter().enumerate() {
        let entry = &mut values[pos / LOGICAL_BLOCK_SIZE as usize];
        *entry = (*entry).max(tf);
    }
    BlockMaxTable::new(list.docids.len() as u32, values)
}

fn verify_build_segment(fx: &ExpandedCorpus) {
    // Inversion contract: sorted distinct docids, positive tfs, occurrence conservation.
    let inverted = invert(&fx.docs, fx.units.len());
    assert_eq!(inverted.len(), fx.units.len());
    let streamed_total: usize = fx.docs.iter().map(Vec::len).sum();
    let inverted_total: usize = inverted
        .iter()
        .map(|list| list.tfs.iter().map(|&tf| tf as usize).sum::<usize>())
        .sum();
    assert_eq!(
        inverted_total, streamed_total,
        "inversion must conserve every expanded token instance"
    );
    for (unit_id, list) in inverted.iter().enumerate() {
        assert!(!list.docids.is_empty(), "unit {unit_id}: empty postings");
        assert_eq!(list.docids.len(), list.tfs.len());
        assert!(
            list.docids.windows(2).all(|w| w[0] < w[1]),
            "unit {unit_id}: docids must ascend strictly"
        );
        // Varint round trip per list before trusting the encoding path.
        let encoded = encode_varint(&list.docids);
        let mut reader = VarintReader::new(&encoded);
        for &docid in &list.docids {
            assert_eq!(reader.next(), Some(docid), "unit {unit_id} round trip");
        }
        assert!(reader.next().is_none(), "unit {unit_id} exhausted");
        // Block-max shape and bounding property.
        let table = build_block_max_table(list);
        assert_eq!(
            table.block_count() as usize,
            logical_block_count(list.docids.len() as u32) as usize
        );
        let max_tf = list.tfs.iter().copied().max().expect("non-empty");
        assert_eq!(
            table.values().iter().copied().max().expect("non-empty"),
            max_tf,
            "unit {unit_id}: block maxima must bound every tf"
        );
    }
    // Dictionary end-to-end on the verification region: insert all dfs, read them all
    // back, then clear so nothing leaks into later state.
    let mut dict = open_dict(VERIFY_DICT_MEMORY_ID);
    dict.clear_new();
    for (unit_id, list) in inverted.iter().enumerate() {
        dict.insert(unit_id as u64, list.docids.len() as u64);
    }
    assert_eq!(dict.len() as usize, inverted.len());
    for (unit_id, list) in inverted.iter().enumerate() {
        assert_eq!(
            dict.get(&(unit_id as u64)),
            Some(list.docids.len() as u64),
            "unit {unit_id}: df mismatch"
        );
    }
    dict.clear_new();
}

/// D1 ingest counterpart to the FTS5 arm's `bench_fts5_ingest_m` (244.27 M instructions
/// at M=2000); no absolute threshold — the FTS5 number is the reference.
///
/// Measured closure = the full physical segment build over the bigram-expanded M=2000
/// corpus ([`crate::expanded`]): invert (docs ascending → per-unit sorted docid lists,
/// dedupe within doc into tf counts) → delta-varint-encode every list → insert unit_id→df
/// entries into a `StableBTreeMap<u64, u64>` → build 128-doc-logical-block block-max
/// tables filled with raw max tf per 128-posting window. Corpus generation and token→unit
/// expansion stay outside the closure. Encoded postings and block-max tables remain
/// heap-resident this slice. The measured SMI reads 0 by construction: the memory manager
/// reserves each region in whole 128-page buckets, and the dictionary region's bucket is
/// claimed when the map handle is opened outside the closure — the same bucketing caveat
/// the FTS5 arm documents for its own SMI; the honest storage numbers are the parity
/// test's byte totals (`storage_parity_full_corpus`).
///
/// **Known asymmetry vs the FTS5 arm:** FTS5 ingest includes its internal tokenization of
/// pre-bigrammed bodies; this bench starts from unit-id streams (the analyzer lives above
/// this crate) and writes through our own structures instead of SQLite's page layer.
#[bench(raw)]
fn bench_build_segment_m2000() -> canbench_rs::BenchResult {
    let fx = black_box(expanded_fixture());
    verify_build_segment(fx);
    let unit_count = fx.units.len();
    let docs = black_box(&fx.docs);
    let mut dict = open_dict(BUILD_DICT_MEMORY_ID);
    assert_eq!(
        dict.len(),
        0,
        "measurement dictionary region must start empty"
    );
    canbench_rs::bench_fn(move || {
        let inverted = invert(docs, unit_count);
        let encoded: Vec<Vec<u8>> = inverted
            .iter()
            .map(|list| encode_varint(&list.docids))
            .collect();
        for (unit_id, list) in inverted.iter().enumerate() {
            dict.insert(unit_id as u64, list.docids.len() as u64);
        }
        let tables: Vec<BlockMaxTable> = inverted.iter().map(build_block_max_table).collect();
        black_box((encoded, tables));
    })
}

// -- dictionary fixture ------------------------------------------------------------------

type DictMap = ic_stable_structures::BTreeMap<
    u64,
    u64,
    ic_stable_structures::memory_manager::VirtualMemory<ic_stable_structures::DefaultMemoryImpl>,
>;

// Host memories are Rc-backed (not Sync), so the singleton lives in thread-local storage;
// benches and canbench both run single-threaded.
thread_local! {
    static DICTIONARY: std::cell::RefCell<Option<DictMap>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` with the dictionary fixture, building it once behind an entry-count guard.
fn with_dictionary<R>(f: impl FnOnce(&DictMap) -> R) -> R {
    DICTIONARY.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let memory = ic_stable_structures::memory_manager::MemoryManager::init(
                ic_stable_structures::DefaultMemoryImpl::default(),
            )
            .get(ic_stable_structures::memory_manager::MemoryId::new(31));
            let mut map = ic_stable_structures::BTreeMap::new(memory);
            if map.len() < DICT_ENTRIES {
                for j in 0..DICT_ENTRIES {
                    let key = j * 2 + (j % u64::from(CORPUS_VOCAB)) % 2;
                    map.insert(key, key.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                }
            }
            assert_eq!(map.len(), DICT_ENTRIES, "dictionary fixture incomplete");
            *slot = Some(map);
        }
        f(slot.as_ref().expect("initialized above"))
    })
}

fn verify_dict() {
    let f = fixture();
    let hits = &f.dict_probes[..DICT_HITS];
    let misses = &f.dict_probes[DICT_HITS..];
    assert_eq!(hits.len(), DICT_HITS);
    assert_eq!(misses.len(), DICT_MISSES);
    with_dictionary(|map| {
        for probe in hits {
            assert!(map.get(probe).is_some(), "hit probe {probe} missing");
        }
        for probe in misses {
            assert!(
                map.get(probe).is_none(),
                "miss probe {probe} unexpectedly present"
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_bands_hold_and_checksums_verify() {
        let f = fixture();
        verify_walk_checksums();
        assert_eq!(f.advance_targets.len(), ADVANCE_CALLS);
        assert!(
            f.advance_targets[0] <= f.plain_a[0],
            "before-first clamp present"
        );
        assert!(
            f.advance_targets[ADVANCE_CALLS - 1] > f.plain_a[f.plain_a.len() - 1],
            "past-end class present"
        );
    }

    #[test]
    fn merge_verification_passes_and_budget_step_matches_prefix() {
        verify_merge();
    }

    #[test]
    fn dict_fixture_probes_are_consistent() {
        verify_dict();
        let f = fixture();
        assert_eq!(f.dict_probes.len(), DICT_HITS + DICT_MISSES);
    }

    #[test]
    fn topk_driver_matches_brute_force_on_the_fixture() {
        verify_topk();
    }

    #[test]
    fn scored_freq_varint_driver_matches_part_model_brute_force() {
        verify_topk_bm25();
    }

    #[test]
    fn build_segment_gate_passes_and_block_max_bounds_every_tf() {
        let fx = expanded_fixture();
        verify_build_segment(fx);
        let inverted = invert(&fx.docs, fx.units.len());
        for list in inverted.iter().take(64) {
            let table = build_block_max_table(list);
            for (block, &bound) in table.values().iter().enumerate() {
                let window_max = list.tfs[block * LOGICAL_BLOCK_SIZE as usize..]
                    .iter()
                    .take(LOGICAL_BLOCK_SIZE as usize)
                    .copied()
                    .max()
                    .expect("blocks only exist when postings fill them");
                assert_eq!(bound, window_max, "stored bound must equal the window max");
            }
        }
    }

    #[test]
    fn brute_force_topk_orders_score_desc_then_docid_asc() {
        let b = vec![1u32, 5, 9];
        let c = vec![5u32, 9, 20];
        let d = vec![9u32];
        let truth = brute_force_topk(&b, &c, &d, &[2, 3, 7], 2);
        assert_eq!(
            truth,
            vec![
                Hit {
                    score: 12,
                    docid: 9
                },
                Hit { score: 5, docid: 5 }
            ]
        );
    }
}
