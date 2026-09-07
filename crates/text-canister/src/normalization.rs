//! Shared text normalization for the text-canister analyzers (plan 0339) — the single
//! normalization choke point for the wired pipelines (id 1 `unicode_bigram`, id 2
//! `mecab`).
//!
//! Two pure, deterministic steps, kept SEPARATE because they apply at different
//! pipeline stages:
//!
//! 1. [`prepass`] — runs BEFORE tokenization: NFKC → Unicode lowercase →
//!    variation-selector strip. The strip is safe pre-tokenization: no ipadic
//!    surface (entry text) contains a variation selector (verified against the
//!    built dictionary by the `analyzer_mecab` test that byte-scans every
//!    valid-UTF-8 field of the dictionary images). Without the strip, 葛󠄀-style
//!    sequences (base + U+E0100) survive NFKC, ride into analysis, break
//!    dictionary/token identity, and drop recall for 人名/地名/official text.
//! 2. [`fold_unit`] / [`fold_units`] — the kana counter-variant unification
//!    ヶ (U+30F6) / ヵ (U+30F5) → ケ (U+30B1) — applied to EMITTED UNITS ONLY,
//!    never to pre-mecab input. ipadic surfaces contain ヶ (茅ヶ崎, 関ヶ原), so
//!    folding pre-tokenization would rewrite the input away from the dictionary
//!    and damage Viterbi matching; folding the emitted lemma/surface units
//!    instead preserves dictionary fidelity while unifying the index/query space.
//!
//! Index/query parity is structural: both sides run the same `analyze` entry
//! points, so both fold identically through these functions. Folding is
//! idempotent and adds no error paths (fail-closed posture unchanged).
//!
//! Migration note (plan 0339): this module lives in text-canister and moves with
//! koine if/when the composite is extracted into a standalone crate.

use unicode_normalization::UnicodeNormalization;

/// Whole-text pre-pass (runs BEFORE tokenization): NFKC → Unicode lowercase →
/// variation-selector strip. Pure and deterministic.
///
/// The NFKC and lowercase order matches the documented v1 pipeline (lowercasing
/// after NFKC); the strip runs last so a selector is removed whether it arrived in
/// the input or survived normalization. The per-character cost on selector-free
/// text is one scan (no reallocation).
pub(crate) fn prepass(text: &str) -> String {
    let normalized = text.nfkc().collect::<String>().to_lowercase();
    if normalized.chars().any(is_variation_selector) {
        normalized
            .chars()
            .filter(|c| !is_variation_selector(*c))
            .collect()
    } else {
        normalized
    }
}

/// Folds the kana counter variants in one emitted unit, in place:
/// ヶ (U+30F6) and ヵ (U+30F5) → ケ (U+30B1). Applied to EMITTED UNITS ONLY (see
/// the module docs for why this must never run pre-tokenization). Idempotent;
/// no-op allocation-wise on units without a variant.
pub(crate) fn fold_unit(unit: &mut String) {
    if unit.contains(['ヶ', 'ヵ']) {
        *unit = unit.replace(['ヶ', 'ヵ'], "ケ");
    }
}

/// Folds the kana counter variants across emitted units, in place.
pub(crate) fn fold_units(units: &mut [String]) {
    for unit in units {
        fold_unit(unit);
    }
}

/// True for every variation selector the pre-pass strips: the general block
/// U+FE00..=U+FE0F, the Mongolian free variation selectors U+180B..=U+180D and
/// U+180F, and the ideographic block U+E0100..=U+E01EF. U+180E is deliberately
/// NOT a variation selector any more (repurposed to a format character in
/// Unicode 6.3) and is not stripped.
fn is_variation_selector(c: char) -> bool {
    matches!(
        c,
        '\u{FE00}'..='\u{FE0F}' | '\u{180B}'..='\u{180D}' | '\u{180F}' | '\u{E0100}'..='\u{E01EF}'
    )
}

/// Scan-facing re-export of [`is_variation_selector`] for cross-module test use
/// (the mecab dictionary field-scan pins the same predicate the strip applies).
#[cfg(test)]
pub(crate) fn is_variation_selector_for_scan(c: char) -> bool {
    is_variation_selector(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- prepass: NFKC + lowercase parity with the former duplicated copies -------------------

    /// IVS-free text is byte-identical to the deleted per-copy behavior
    /// (NFKC + lowercase, nothing else). The analyzer fixtures pin the same
    /// property at the emission level.
    #[test]
    fn prepass_is_nfkc_lowercase_on_selector_free_text() {
        assert_eq!(prepass("Ｈｅｌｌｏ Ｗｏｒｌｄ"), "hello world");
        // Composition crosses characters (the analyzer's whole-segment contract).
        assert_eq!(prepass("ﾊﾟﾈﾙ"), "パネル");
        assert_eq!(prepass("㍿"), "株式会社");
        assert_eq!(prepass("ﬁn"), "fin");
        assert_eq!(prepass("ＦＵＬＬＴＥＸＴ ①②"), "fulltext 12");
        assert_eq!(prepass("走った。"), "走った。");
    }

    #[test]
    fn prepass_strips_variation_selectors() {
        // Ideographic VS (U+E0100..=U+E01EF): the 人名/地名 hazard.
        assert_eq!(prepass("葛\u{E0100}"), "葛");
        assert_eq!(prepass("葛\u{E0101}飾"), "葛飾");
        assert_eq!(prepass("邊\u{E01EF}"), "邊");
        // General VS block (a base that NFKC leaves alone, so the strip is the only
        // actor): ☂ + VS16.
        assert_eq!(prepass("☂\u{FE0F}"), "☂");
        // Mongolian free variation selectors.
        assert_eq!(prepass("\u{1820}\u{180B}"), "\u{1820}");
        assert_eq!(prepass("\u{1820}\u{180D}"), "\u{1820}");
        assert_eq!(prepass("\u{1820}\u{180F}"), "\u{1820}");
        // Multiple selectors and both edges of the text.
        assert_eq!(prepass("\u{E0100}葛\u{FE0F}\u{E0100}"), "葛");
    }

    #[test]
    fn prepass_keeps_mongolian_vowel_separator() {
        // U+180E was a variation selector until Unicode 6.3 and is now a format
        // character — the strip must NOT remove it (boundary pin).
        assert_eq!(prepass("\u{1820}\u{180E}"), "\u{1820}\u{180E}");
    }

    #[test]
    fn prepass_is_idempotent() {
        let fixtures = [
            "",
            "Ｈｅｌｌｏ ①②",
            "ﾊﾟﾈﾙ ㍿",
            "葛\u{E0100}飾区",
            "\u{1820}\u{180B}\u{180E}",
            "3ヶ月 ３ケ月 16ヵ所",
        ];
        for fixture in fixtures {
            let once = prepass(fixture);
            assert_eq!(
                prepass(&once),
                once,
                "prepass must be idempotent on {fixture:?}"
            );
        }
    }

    // -- fold: the kana counter-variant matrix ------------------------------------------------

    #[test]
    fn fold_unifies_counter_kana_variants() {
        let fold = |s: &str| {
            let mut unit = s.to_string();
            fold_unit(&mut unit);
            unit
        };
        assert_eq!(fold("ヶ"), "ケ");
        assert_eq!(fold("ヵ"), "ケ");
        assert_eq!(fold("ケ"), "ケ"); // already the target
        assert_eq!(fold("一ヶ月"), "一ケ月");
        assert_eq!(fold("16ヵ所"), "16ケ所");
        assert_eq!(fold("茅ヶ崎"), "茅ケ崎");
        assert_eq!(fold("3ヶ月ヵ所"), "3ケ月ケ所");
    }

    #[test]
    fn fold_does_not_touch_non_counter_kana() {
        let fold = |s: &str| {
            let mut unit = s.to_string();
            fold_unit(&mut unit);
            unit
        };
        // The v1 boundary: hiragana か and the voiced/other katakana stay put —
        // unconditional か→ケ would merge the particle か (plan 0339 deferral).
        assert_eq!(fold("か"), "か");
        assert_eq!(fold("か月"), "か月");
        assert_eq!(fold("カ"), "カ");
        assert_eq!(fold("が"), "が");
        assert_eq!(fold("ガ"), "ガ");
        assert_eq!(fold("走った"), "走った");
    }

    #[test]
    fn fold_is_idempotent() {
        let fixtures = [
            "",
            "ヶ",
            "ヵ",
            "ケ",
            "3ヶ月",
            "16ヵ所",
            "茅ヶ崎",
            "か月",
            "ヶヶ",
        ];
        for fixture in fixtures {
            let mut once = fixture.to_string();
            fold_unit(&mut once);
            let mut twice = once.clone();
            fold_unit(&mut twice);
            assert_eq!(twice, once, "fold must be idempotent on {fixture:?}");
        }
    }

    #[test]
    fn fold_units_folds_every_emitted_unit() {
        let mut units = vec!["3".to_string(), "ヶ月".to_string(), "か月".to_string()];
        fold_units(&mut units);
        assert_eq!(units, ["3", "ケ月", "か月"]);
    }

    /// The composed pipeline (prepass then fold) unifies the fullwidth/kana
    /// counter spellings: ３ヶ月 and ３ケ月 and 3ヶ月 all land on the same string.
    #[test]
    fn prepass_then_fold_composes_fullwidth_and_kana_chain() {
        let chain = |s: &str| {
            let mut unit = prepass(s);
            fold_unit(&mut unit);
            unit
        };
        assert_eq!(chain("３ヶ月"), "3ケ月");
        assert_eq!(chain("３ケ月"), "3ケ月");
        assert_eq!(chain("3ヶ月"), "3ケ月");
        // The v1 boundary survives the chain.
        assert_eq!(chain("３か月"), "3か月");
    }
}
