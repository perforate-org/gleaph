//! Production text analyzer (the ADR 0077 default pipeline).
//!
//! Pipeline: UAX #29 word boundaries over the raw text, then per segment NFKC
//! normalization followed by Unicode lowercasing, then CJK-run expansion — contiguous
//! CJK characters (Hiragana, Katakana, CJK Unified Ideographs) become overlapping
//! bigrams while a lone CJK character stays a unigram; every other word passes through
//! whole. Segments consisting purely of separators/symbols are dropped, which also makes
//! NFKC expansions of symbols (e.g. ㍿ → 株式会社) indexable. Output units are a
//! deterministic function of the input, and analysis is idempotent:
//! `analyze(&units.join(" ")) == analyze(analyze_input)` holds for every fixture.
//!
//! This module supersedes the PoC corpus fixture helpers
//! (`ic_stable_text_postings::corpus::{is_cjk_token, expand_bigrams}`) for production:
//! those exist only to synthesize benchmark corpora and must not be reused here. The
//! pipeline identity is recorded as [`ANALYZER_ID`] in the index meta cell so index
//! definitions can pin the exact analyzer that produced their postings.

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

/// Registered identity of the script-dispatched multilingual composite (plan 0332,
/// branded `koine`). DDL identifier: `multilingual`. The DEFAULT for any absent
/// `ANALYZER` clause and the canonical install default. Carries the dictionary
/// machinery (the same MPD container as id 2) AND the per-script rule layers
/// (Hangul 조사/어미, Latin Porter, pure-Han bigram fallback).
pub const ANALYZER_MULTILINGUAL: u32 = 0;
/// Registered identity of the unicode-bigram pipeline (segmentation + NFKC + lowercase +
/// CJK bigrams). Registered identities are creation-fixed per index (plan 0331); ids stay
/// internal — DDL names resolve at admission.
pub const ANALYZER_UNICODE_BIGRAM: u32 = 1;
/// Registered identity of the MeCab-format ipadic lemma pipeline (plan 0330 decision B,
/// plan 0333 proof, plan 0334 landing). Requires the stable-resident MeCab-format
/// dictionary container to be finalized before any analyze-touching operation.
pub const ANALYZER_MECAB: u32 = 2;
/// Deprecated alias of [`ANALYZER_UNICODE_BIGRAM`] kept for the v1 references across
/// meta defaults, backfill scope validation, and the Router's v0 admission constant.
#[deprecated(since = "0.1.0", note = "use ANALYZER_UNICODE_BIGRAM or the dispatch")]
pub const ANALYZER_ID: u32 = ANALYZER_UNICODE_BIGRAM;

/// Dispatches one analysis by pinned pipeline id (plan 0331, plan 0332 widening). Ids
/// are open-validated at the open/admission boundaries, so an unknown id reaching here
/// is a broken invariant: fail closed loudly instead of silently misanalyzing.
pub fn analyze_pinned(id: u32, text: &str) -> Vec<String> {
    match id {
        ANALYZER_MULTILINGUAL => crate::analyzer_multilingual::analyze(text),
        ANALYZER_UNICODE_BIGRAM => analyze(text),
        ANALYZER_MECAB => crate::analyzer_mecab::analyze(text),
        other => unreachable!(
            "unregistered analyzer id {other} passed open validation — broken invariant"
        ),
    }
}

/// True for ids that carry the stable-resident MPD dictionary container. Drives the
/// dictionary upload / backfill-hold / open-rebind gates (plan 0332: id 0 and id 2
/// both go through the same dictionary machinery; id 1 is dictionary-free). The
/// `DICT_REQUIRED` gate is the single source of truth.
pub const fn dict_required(id: u32) -> bool {
    matches!(id, ANALYZER_MULTILINGUAL | ANALYZER_MECAB)
}

/// True for characters eligible for CJK-run bigram expansion: Hiragana
/// U+3041..=U+3096, Katakana U+30A1..=U+30FF, and CJK Unified Ideographs
/// U+4E00..=U+9FFF — the same classes the PoC corpus fixtures model.
fn is_cjk_char(c: char) -> bool {
    matches!(
        c,
        '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}'
    )
}

/// True for characters kept inside a word token: alphanumeric characters plus the
/// connecting underscore (UAX #29 ExtendNumLet).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Tokenizes `text` into indexable units. Order follows input order and duplicates are
/// preserved; callers count occurrences to derive term frequencies.
pub fn analyze(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Contiguous CJK characters awaiting bigram expansion. UAX #29 splits Han/Kana runs
    // per character, so the run accumulates across adjacent segments; a gap (any
    // intervening separator bytes) ends it via the adjacency check below.
    let mut run = String::new();
    // Current non-CJK word; never crosses a word-boundary segment.
    let mut token = String::new();
    let mut prev_segment_end: Option<usize> = None;
    for (start, segment) in text.split_word_bound_indices() {
        // NFKC must see the whole segment: canonical composition crosses characters
        // (e.g. halfwidth ﾊ + ﾟ → パ, ㍿ → 株式会社). Lowercasing after NFKC matches the
        // documented pipeline order.
        let normalized = segment.nfkc().collect::<String>().to_lowercase();
        if !normalized.chars().any(is_word_char) {
            continue; // pure separator/symbol segment; the gap breaks CJK adjacency
        }
        if prev_segment_end != Some(start) {
            flush_run(&mut out, &mut run);
        }
        for c in normalized.chars() {
            if is_cjk_char(c) {
                flush_token(&mut out, &mut token);
                run.push(c);
            } else {
                flush_run(&mut out, &mut run);
                token.push(c);
            }
        }
        flush_token(&mut out, &mut token);
        prev_segment_end = Some(start + segment.len());
    }
    flush_run(&mut out, &mut run);
    out
}

/// Bigram-expands an accumulated contiguous CJK run into `out` and clears it.
fn flush_run(out: &mut Vec<String>, run: &mut String) {
    if !run.is_empty() {
        expand_cjk_run(out, run);
        run.clear();
    }
}

/// Emits the assembled non-CJK word and clears it.
fn flush_token(out: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        out.push(std::mem::take(token));
    }
}

/// Expands one contiguous CJK run: overlapping character bigrams, with a lone character
/// staying a unigram.
fn expand_cjk_run(out: &mut Vec<String>, run: &str) {
    let chars: Vec<char> = run.chars().collect();
    match chars.as_slice() {
        [c] => out.push(c.to_string()),
        _ => out.extend(chars.windows(2).map(|pair| pair.iter().collect())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> Vec<String> {
        analyze(text)
    }

    #[test]
    fn ascii_words_are_lowercased_and_split_on_punctuation() {
        assert_eq!(units("Hello, World!"), vec!["hello", "world"]);
        assert_eq!(units("FULLTEXT v2"), vec!["fulltext", "v2"]);
    }

    #[test]
    fn nfkc_folds_compatibility_forms_to_ascii() {
        assert_eq!(units("Ｈｅｌｌｏ Ｗｏｒｌｄ"), vec!["hello", "world"]);
        assert_eq!(units("ﬁn"), vec!["fin"]);
    }

    #[test]
    fn cjk_runs_expand_to_overlapping_bigrams() {
        assert_eq!(units("東京都"), vec!["東京", "京都"]);
        assert_eq!(units("あいうえお"), vec!["あい", "いう", "うえ", "えお"]);
    }

    #[test]
    fn lone_cjk_characters_stay_unigrams() {
        assert_eq!(units("京"), vec!["京"]);
        assert_eq!(units("都 京都"), vec!["都", "京都"]);
    }

    #[test]
    fn mixed_script_document_fixture() {
        // Halfwidth katakana ﾊﾟﾈﾙ needs whole-word NFKC (composition crosses chars:
        // ﾊ + ﾟ → パ) before the Katakana run bigrams; the Han run spans three UAX #29
        // segments but stays one contiguous run.
        assert_eq!(
            units("GQL 東京都 FULLTEXT ﾊﾟﾈﾙ v2"),
            vec!["gql", "東京", "京都", "fulltext", "パネ", "ネル", "v2"]
        );
    }

    #[test]
    fn nfkc_expansion_of_one_symbol_becomes_a_cjk_run() {
        // ㍿ normalizes to four Han characters, which then bigram like any other run.
        assert_eq!(units("㍿"), vec!["株式", "式会", "会社"]);
    }

    #[test]
    fn re_analysis_is_idempotent() {
        let fixtures = [
            "",
            "Hello, World!",
            "東京都",
            "GQL 東京都 FULLTEXT ﾊﾟﾈﾙ v2",
            "Ｈｅｌｌｏ ①② 東 京都 visit",
            "red RED Red",
            "x3y 漢字a b漢字",
        ];
        for fixture in fixtures {
            let first = units(fixture);
            let second = units(&first.join(" "));
            assert_eq!(
                second, first,
                "re-analysis of {fixture:?} must be idempotent"
            );
        }
    }

    #[test]
    fn symbol_only_and_empty_inputs_yield_no_units() {
        assert!(units("").is_empty());
        assert!(units("!!! ... ??? ---").is_empty());
    }

    #[test]
    fn duplicates_are_preserved_for_tf_counting() {
        assert_eq!(units("red red red"), vec!["red"; 3]);
        assert_eq!(units("東京都 東京都"), vec!["東京", "京都", "東京", "京都"]);
    }

    // -- Plan 0332 regression fixtures: ids 1 and 2 stay byte-unchanged -------------------

    /// The id-1 dispatch is byte-identical to the v1 direct call (contract frozen:
    /// the composite promotion must never change what id 1 emits).
    #[test]
    fn id1_dispatch_matches_v1_pipeline_exactly() {
        let fixtures = [
            "Hello, World!",
            "東京都",
            "GQL 東京都 FULLTEXT ﾊﾟﾈﾙ v2",
            "running 학교에서 数据库",
            "Ｈｅｌｌｏ ①② 東 京都 visit",
        ];
        for fixture in fixtures {
            assert_eq!(
                analyze_pinned(ANALYZER_UNICODE_BIGRAM, fixture),
                analyze(fixture),
                "id-1 dispatch must equal the v1 pipeline on {fixture:?}"
            );
        }
        // The id-1 units are pure bigram: NO rule layers (no Porter stem, no 조사
        // strip) leak into id 1.
        assert_eq!(
            analyze_pinned(ANALYZER_UNICODE_BIGRAM, "running"),
            vec!["running"],
            "id 1 must NOT stem Latin words"
        );
        assert_eq!(
            analyze_pinned(ANALYZER_UNICODE_BIGRAM, "학교에서"),
            vec!["학교에서"],
            "id 1 must NOT strip 조사"
        );
    }

    /// The id-2 dispatch is byte-identical to the whole-text mecab call (contract
    /// frozen). Requires the dictionary when present (skipped loudly otherwise);
    /// the dispatch wiring itself is checked with the dictionary-free fixtures.
    #[test]
    fn id2_dispatch_is_whole_text_mecab() {
        // Without the dictionary resident, id-2 dispatch panics fail-closed — the
        // same contract as before the composite landed.
        if !crate::analyzer_mecab::dictionary_loaded() {
            // dispatch wiring: id 2 routes to analyzer_mecab::analyze (panics); id 0
            // routes to the composite (Hangul layer works WITHOUT the dictionary —
            // the distinguishing observable between the two dictionary-carrying ids).
            assert_eq!(
                analyze_pinned(ANALYZER_MULTILINGUAL, "학교에서"),
                vec!["학교에서", "학교"],
                "id-0 Hangul layer works without the dictionary (rule layer)"
            );
            let result = std::panic::catch_unwind(|| analyze_pinned(ANALYZER_MECAB, "走った"));
            assert!(
                result.is_err(),
                "id-2 without the dictionary must fail closed"
            );
            return;
        }
        assert_eq!(
            analyze_pinned(ANALYZER_MECAB, "走った"),
            crate::analyzer_mecab::analyze("走った"),
            "id-2 dispatch must equal the whole-text mecab call"
        );
    }

    /// The dictionary-carrying id set is exactly {0, 2}; id 1 is dictionary-free.
    #[test]
    fn dict_required_gate_is_exactly_ids_0_and_2() {
        assert!(dict_required(ANALYZER_MULTILINGUAL));
        assert!(!dict_required(ANALYZER_UNICODE_BIGRAM));
        assert!(dict_required(ANALYZER_MECAB));
        assert!(!dict_required(3));
        assert!(!dict_required(u32::MAX));
    }
}
