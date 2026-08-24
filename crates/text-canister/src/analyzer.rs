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

/// Registered identity of this analyzer (segmentation + NFKC + lowercase + CJK bigrams).
/// Morphological (lindera) analyzers will take later ids behind their feature flag.
pub const ANALYZER_ID: u32 = 1;

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
}
