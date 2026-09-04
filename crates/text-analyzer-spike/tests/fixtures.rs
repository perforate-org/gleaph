//! Recall / determinism / idempotence / overmatch fixtures for plan 0330.
//!
//! Runs per enabled candidate feature; `candidates()` only sees compiled-in
//! candidates, so each measurement run enables exactly one candidate feature
//! (see build_wasm.sh / the plan's Audit section for the matrix).

use text_analyzer_spike::harness::candidates;

// -- Fixtures --------------------------------------------------------------------------

fn shared(a: &str, b: &str, analyze: &dyn Fn(&str) -> Vec<String>) -> Vec<String> {
    let units_b: std::collections::BTreeSet<String> = analyze(b).into_iter().collect();
    analyze(a)
        .into_iter()
        .filter(|u| units_b.contains(u))
        .collect()
}

/// LEMMA RECALL (gate 1): doc 走った ⇄ query 走る must share ≥1 unit for every candidate.
#[test]
fn lemma_recall_hashitta_matches_hashiru() {
    let doc = "昨日、公園を全力で走った。";
    let query = "走る";
    for (name, analyze) in candidates() {
        let hits = shared(doc, query, analyze.as_ref());
        assert!(
            !hits.is_empty(),
            "candidate {name}: doc 走った and query 走る must share a unit, doc={:?} query={:?}",
            analyze(doc),
            analyze(query)
        );
        println!("lemma-recall[{name}]: shared units {hits:?}");
    }
}

/// 表記ゆれ (附属 ⇄ 付属): expected PASS only for candidate C (sudachi normalized form).
/// Every other candidate records the EXPECTED FAILURE (documented recall gap).
#[test]
fn hyouki_yure_fuzoku_expected_pass_only_for_sudachi() {
    let doc = "附属病院";
    let query = "付属";
    for (name, analyze) in candidates() {
        let hits = shared(doc, query, analyze.as_ref());
        if name == "sudachi" {
            assert!(
                !hits.is_empty(),
                "candidate {name}: normalized form must bridge 附属⇄付属, doc={:?} query={:?}",
                analyze(doc),
                analyze(query)
            );
        } else {
            assert!(
                hits.is_empty(),
                "candidate {name}: 表記ゆれ is a documented NON-goal for this candidate \
                 (only candidate C solves it) — unexpected match {hits:?}",
            );
        }
        println!("hyouki-yure[{name}]: shared units {hits:?} (expected empty for non-C)");
    }
}

/// DETERMINISM (gate 2): the same input analyzed twice yields identical output.
#[test]
fn determinism_same_input_twice() {
    let samples = [
        "昨日、公園を全力で走った。",
        "附属病院の付属図書館",
        "本とカレーの街神保町へようこそ。",
    ];
    for (name, analyze) in candidates() {
        for sample in samples {
            assert_eq!(
                (analyze)(sample),
                (analyze)(sample),
                "candidate {name} is not deterministic on {sample:?}"
            );
        }
    }
}

/// IDEMPOTENCE (gate 2): `analyze(units.join(" ")) == analyze(input)`.
#[test]
fn idempotence_reanalysis_is_a_fixed_point() {
    let samples = [
        "昨日、公園を全力で走った。",
        "附属病院の付属図書館",
        "走った走った走った",
    ];
    for (name, analyze) in candidates() {
        for sample in samples {
            let units = analyze(sample);
            let joined = units.join(" ");
            assert_eq!(
                analyze(&joined),
                units,
                "candidate {name}: re-analysis of the joined unit stream must be a fixed point; \
                 input={sample:?} units={units:?} reanalyzed={:?}",
                analyze(&joined)
            );
        }
        println!("idempotence[{name}]: strict fixed point holds on the fixture set");
    }
}

/// Candidate A overmatch table (gate 1 negative): query 走る must NOT match the
/// unrelated 走行-class docs through the stem unit; the measured table goes to the plan.
#[cfg(feature = "rule")]
#[test]
fn rule_candidate_overmatch_table() {
    let analyze = text_analyzer_spike::rule::analyze;
    let query = "走る";
    let unrelated_docs = ["走行", "走査", "走塁", "走路", "走者"];
    for doc in unrelated_docs {
        let hits = shared(doc, query, &analyze);
        assert!(
            hits.is_empty(),
            "overmatch: query 走る must not match unrelated doc {doc:?}, shared={hits:?}"
        );
    }
    // Related inflection forms DO match through the stem (the intended recall).
    for doc in ["走った", "走ります", "走りたい", "走ろう"] {
        let hits = shared(doc, query, &analyze);
        assert!(!hits.is_empty(), "stem recall lost for {doc:?}");
    }
    println!("overmatch-table[rule]: unrelated 走行-class docs unmatched, inflected forms matched");
}

/// Unit-count sanity (MAX_UNITS_PER_DOC scale): a long document produces a bounded,
/// order-of-magnitude sane unit count (no combinatorial blowup).
#[test]
fn unit_count_sanity_no_blowup() {
    let paragraph = "昨日、公園を全力で走った。今日も附属病院の前を走ります。";
    let long_text = paragraph.repeat(100);
    for (name, analyze) in candidates() {
        let units = (analyze)(long_text.as_str());
        // ≤ 8 units per 20-char paragraph keeps us far below MAX_UNITS_PER_DOC scale.
        assert!(
            units.len() <= long_text.chars().count(),
            "candidate {name}: unit blowup, {} units for {} chars",
            units.len(),
            long_text.chars().count()
        );
        println!(
            "unit-count[{name}]: {} units / {} chars",
            units.len(),
            long_text.chars().count()
        );
    }
}

/// Candidate D embedded-dictionary path: `embedded://ipadic` must load a REAL
/// dictionary (lindera's build.rs silently falls back to a dummy dictionary when the
/// source fetch fails — this test would catch that).
#[cfg(feature = "lindera")]
#[test]
fn lindera_embedded_dictionary_loads_and_recalls() {
    let analyzer = text_analyzer_spike::lindera_candidate::Analyzer::embedded()
        .expect("embedded ipadic dictionary load");
    let units = analyzer.analyze("昨日、公園を全力で走った。");
    println!("lindera-embedded units: {units:?}");
    assert!(
        units.iter().any(|u| u == "走る"),
        "embedded ipadic must be a real dictionary (走った -> 走る lemma), got {units:?}"
    );
}

/// The shared pre-pass mirrors the v1 analyzer's NFKC + lowercase ordering.
#[test]
fn prepass_nfkc_and_lowercase() {
    assert_eq!(text_analyzer_spike::prepass("ＡＢＣｱｲｳ"), "abcアイウ");
    assert_eq!(text_analyzer_spike::prepass("走った。"), "走った。");
}
