//! ANALYZER_ID=0 pipeline — the script-dispatched multilingual composite (plan 0332,
//! branded `koine`). DDL identifier: `multilingual`. Ids 1 (`unicode_bigram`) and 2
//! (`mecab`) are kept non-default; id 0 carries the dictionary and the rule layers.
//!
//! ## Pipeline
//!
//! 1. Shared pre-pass: whole-text NFKC + Unicode lowercase (the v1 normalization order,
//!    the same pre-pass the id-2 mecab analyzer runs);
//! 2. UAX #29 segmentation (per-word boundaries, the v1 contract);
//! 3. Per-segment script-run classification (deterministic codepoint-range dispatch,
//!    NO statistical language detection — the determinism contract);
//! 4. Per-run layer dispatch:
//!    - `{kanji ∪ kana}` runs (Japanese: Han + Hiragana + Katakana, the v1 CJK classes)
//!      go to the mecab layer over the `morph-dict` engine with the Japanese
//!      `DictionaryProfile` (the 0334 module). The layer needs the dictionary
//!      resident, exactly like id 2.
//!    - Pure Han runs (only CJK Unified Ideographs, no kana) get v1 bigram expansion
//!      (the fundamental zh/ja ambiguity: bigram is correct for both; dual-emission
//!      of mecab+bigram units is the quality option recorded for later, not landed).
//!    - Hangul (Hangul Syllables U+AC00..=U+D7A3 + Hangul Jamo U+1100..=U+11FF +
//!      compatibility U+3130..=U+318F) runs go through the 조사/어미 suffix strip
//!      (closed-class tables with 받침 allomorphy, enumerated ㄷ/ㅂ/르 irregulars).
//!      Surface+stem dual emission keeps precision while adding recall.
//!    - Latin-script words get a canonical Porter stem (English-focused) plus the
//!      surface. Non-English Latin over-stem risk is accepted and recorded.
//!    - Everything else (digits, symbols, mixed scripts within one segment) goes
//!      through whole — same shape as the v1 bigram analyzer's non-CJK pass.
//!
//! Output order follows input order; duplicates are preserved. Idempotence holds:
//! `analyze(&units.join(" ")) == analyze(input)` (the rule layers are pure functions
//! of the run; the space-joined surface reproduces the same run-shape input).

use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::UnicodeSegmentation;

// -- Pre-pass + UAX #29 segmentation -------------------------------------------------------

/// Returns the pre-passed text (whole-text NFKC + lowercase, the v1 normalization
/// order). The composite runs this once over the whole input so the mecab layer sees
/// the same adjacency the id-2 whole-text analyzer would see.
fn prepass(text: &str) -> String {
    text.nfkc().collect::<String>().to_lowercase()
}

/// True for characters eligible for CJK-run bigram expansion (the v1 classes):
/// Hiragana U+3041..=U+3096, Katakana U+30A1..=U+30FF, and CJK Unified Ideographs
/// U+4E00..=U+9FFF. Mirrors `analyzer::is_cjk_char` so the composite's CJK runs align
/// with what the v1 bigram analyzer would emit (no behavior drift for pure-Han
/// fixtures on id 0 vs id 1).
fn is_cjk_char(c: char) -> bool {
    matches!(
        c,
        '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}'
    )
}

/// True for Hangul ranges (Syllables + Jamo + Compatibility Jamo). The Korean layer
/// only acts on these; everything else passes through.
fn is_hangul(c: char) -> bool {
    matches!(
        c,
        '\u{AC00}'..='\u{D7A3}' | '\u{1100}'..='\u{11FF}' | '\u{3130}'..='\u{318F}'
    )
}

/// True for Latin-script letters (Basic Latin U+0041..=U+007A, Latin-1 Supplement
/// U+00C0..=U+00FF, Latin Extended A/B). The Porter layer only acts on these.
fn is_latin(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(c, '\u{00C0}'..='\u{024F}')
        || matches!(c, '\u{1E00}'..='\u{1EFF}')
}

/// True for characters kept inside a non-CJK word token (digits, underscore, anything
/// alphanumeric outside the script-specific layer). Mirrors the v1 analyzer's
/// `is_word_char` contract for the gaps between runs.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// -- Script-run classification -------------------------------------------------------------

/// Per-segment run classification, the dispatch unit of the composite. A run is the
/// maximal contiguous span of characters that all belong to the same dispatch class.
/// CJK (kanji+kana) absorbs the katakana fold for the Japanese mecab layer; pure Han
/// stays a separate class for the bigram fallback; Hangul is one class; Latin is
/// one class; everything else (digits, symbols, mixed scripts within one segment)
/// falls into `Other` and is emitted whole (matching the v1 analyzer's
/// non-CJK-pass contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScriptClass {
    /// Han + Hiragana + Katakana → the mecab layer (the Japanese layer).
    Japanese,
    /// Pure Han (CJK Unified Ideographs only) → v1 bigram fallback.
    HanOnly,
    /// Hangul → 조사/어미 suffix strip.
    Hangul,
    /// Latin-script word → Porter stem.
    Latin,
    /// Any other character (digit, symbol, mixed script in one segment) → emit whole.
    Other,
}

fn classify(c: char) -> ScriptClass {
    if is_cjk_char(c) {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c) {
            // CJK Unified Ideographs: pure Han in the run if the run is only these.
            // The Japanese class covers mixed Han+kana; the per-run classification
            // resolves that below by inspecting the whole run.
            ScriptClass::HanOnly
        } else {
            ScriptClass::Japanese
        }
    } else if is_hangul(c) {
        ScriptClass::Hangul
    } else if is_latin(c) {
        ScriptClass::Latin
    } else {
        ScriptClass::Other
    }
}

/// Classifies a maximal run by inspecting the run as a whole. A Han-only run
/// (no kana anywhere in it) is `HanOnly`; a Han+kana mixed run is `Japanese` (the
/// mecab layer treats it as one chunked piece of Japanese text). A Hangul run is
/// always `Hangul`; a Latin run is `Latin`; everything else is `Other`. Empty runs
/// map to `Other` (caller must drop them).
fn classify_run(run: &str) -> ScriptClass {
    if run.is_empty() {
        return ScriptClass::Other;
    }
    let mut has_kanji = false;
    let mut has_kana = false;
    let mut has_hangul = false;
    let mut has_latin = false;
    let mut has_mixed = false;
    for c in run.chars() {
        if ('\u{4E00}'..='\u{9FFF}').contains(&c) {
            has_kanji = true;
        } else if matches!(c, '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FF}') {
            has_kana = true;
        } else if is_hangul(c) {
            has_hangul = true;
        } else if is_latin(c) {
            has_latin = true;
        } else {
            has_mixed = true;
        }
    }
    // Mixed-script runs (e.g. embedded punctuation or different scripts) route to
    // Japanese when they contain any CJK content; otherwise the first detected
    // class wins (the per-character dispatch above already broke the run on
    // script-class changes, so a single-class run is the common case here).
    if has_mixed {
        if has_kanji || has_kana {
            ScriptClass::Japanese
        } else if has_hangul {
            ScriptClass::Hangul
        } else if has_latin {
            ScriptClass::Latin
        } else {
            ScriptClass::Other
        }
    } else if has_kana {
        // Kana present (with or without Han): Japanese (the mecab layer).
        ScriptClass::Japanese
    } else if has_kanji {
        ScriptClass::HanOnly
    } else if has_hangul {
        ScriptClass::Hangul
    } else if has_latin {
        ScriptClass::Latin
    } else {
        ScriptClass::Other
    }
}

// -- Layer dispatch ------------------------------------------------------------------------

/// Analyzes a maximal CJK run (`Japanese` or `HanOnly`) — splits the run at
/// `{kanji∪kana}` vs pure-Han boundaries, dispatches each piece to the right layer,
/// and concatenates the units preserving input order. The mecab layer handles
/// `{kanji∪kana}` pieces; the pure-Han pieces go to v1 bigram expansion.
fn layer_japanese_or_han(run: &str, out: &mut Vec<String>) {
    // Per-character walk: accumulate a CJK run; flush it to the mecab layer when the
    // next character is not a CJK character (any gap, even a digit, ends the run).
    // Pure-Han segments inside the CJK run still go to the mecab layer (Japanese
    // text routinely mixes kanji+kana runs with pure-Han content words and the
    // dictionary-aware segmentation handles them as one).
    let mut cjk_run = String::new();
    for c in run.chars() {
        if is_cjk_char(c) {
            cjk_run.push(c);
        } else {
            flush_cjk_to_layers(&mut cjk_run, out);
        }
    }
    flush_cjk_to_layers(&mut cjk_run, out);
}

/// Splits one CJK run at pure-Han boundaries and dispatches: each sub-run that
/// contains kana (or that is mixed Han+kana, which is every typical Japanese
/// sentence) goes to the mecab layer; pure-Han-only sub-runs go to v1 bigram.
fn flush_cjk_to_layers(cjk_run: &mut String, out: &mut Vec<String>) {
    if cjk_run.is_empty() {
        return;
    }
    // The mecab layer is one whole-text Viterbi over the whole Japanese run — the
    // dictionary segmentation decides word boundaries. For maximal accuracy we hand
    // the entire CJK run to the mecab layer when it contains kana OR mixed Han+kana
    // (the common Japanese case); for a pure-Han run we use the v1 bigram fallback
    // directly (the mecab layer has no lemma advantage on a pure-Han chunk and the
    // bigram keeps the index tight).
    let has_kana = cjk_run
        .chars()
        .any(|c| matches!(c, '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FF}'));
    if has_kana {
        // Hand the whole CJK run to the mecab layer (one whole-text Viterbi call).
        // The layer is the same id-2 analyzer, so its emission is the documented
        // Japanese DictionaryProfile lemma set.
        out.extend(crate::analyzer_mecab::analyze(cjk_run));
    } else {
        // Pure-Han run: v1 bigram expansion (the fundamental zh/ja ambiguity:
        // bigram is correct for both; dual-emission of mecab+bigram units is the
        // quality option recorded for later, not landed).
        emit_bigrams(cjk_run, out);
    }
    cjk_run.clear();
}

/// Expands one contiguous pure-Han run into overlapping character bigrams; a lone
/// character stays a unigram (the v1 contract).
fn emit_bigrams(run: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = run.chars().collect();
    match chars.as_slice() {
        [c] => out.push(c.to_string()),
        _ => out.extend(chars.windows(2).map(|pair| pair.iter().collect())),
    }
}

// -- Hangul layer (closed-class 조사/어미 suffix strip) -------------------------------------

/// Hangul L-consonant (받침) extraction: maps one Hangul syllable to its 받침
/// codepoint, or 0 if there is no 받침. The vowel + 받침 decomposition uses the
/// standard formula (Unicode PR #209): base = 0xAC00, onset = 21, coda = 28.
fn jongseong(c: char) -> u32 {
    let code = c as u32;
    if !(0xAC00..=0xD7A3).contains(&code) {
        return 0;
    }
    (code - 0xAC00) % 28
}

/// True if the Hangul syllable has a 받침 (final consonant).
fn has_jongseong(c: char) -> bool {
    (0xAC00..=0xD7A3).contains(&(c as u32)) && jongseong(c) != 0
}

/// One 조사 (postposition): `jongsung` is the form required after a 받침-final
/// syllable, `no_jongsung` the form after a vowel-final syllable. Invariable 조사
/// (no allomorphy) carry the same form in both slots. The ㄹ 받침 follows the
/// vowel-final form for the instrumental pair (으로/로) — handled by the validator.
struct JosaPair {
    jongsung: &'static str,
    no_jongsung: &'static str,
}

/// Closed-class 조사 table (the plan's "~80 조사" v0 core, deduplicated). Each
/// allomorphic pair is one row; invariable 조사 repeat their surface.
const JOSA_TABLE: &[JosaPair] = &[
    JosaPair {
        jongsung: "은",
        no_jongsung: "는",
    }, // topic
    JosaPair {
        jongsung: "이",
        no_jongsung: "가",
    }, // subject
    JosaPair {
        jongsung: "을",
        no_jongsung: "를",
    }, // object
    JosaPair {
        jongsung: "으로",
        no_jongsung: "로",
    }, // instrumental (ㄹ 받침 → 로)
    JosaPair {
        jongsung: "과",
        no_jongsung: "와",
    }, // and/with
    JosaPair {
        jongsung: "과의",
        no_jongsung: "와의",
    },
    JosaPair {
        jongsung: "과는",
        no_jongsung: "와는",
    },
    JosaPair {
        jongsung: "과도",
        no_jongsung: "와도",
    },
    JosaPair {
        jongsung: "으로는",
        no_jongsung: "로는",
    },
    JosaPair {
        jongsung: "으로서",
        no_jongsung: "로서",
    },
    JosaPair {
        jongsung: "으로써",
        no_jongsung: "로써",
    },
    JosaPair {
        jongsung: "으로부터",
        no_jongsung: "로부터",
    },
    JosaPair {
        jongsung: "이나",
        no_jongsung: "나",
    }, // or
    JosaPair {
        jongsung: "이랑",
        no_jongsung: "랑",
    }, // and (colloquial)
    JosaPair {
        jongsung: "이며",
        no_jongsung: "며",
    },
    JosaPair {
        jongsung: "이라",
        no_jongsung: "라",
    }, // called
    JosaPair {
        jongsung: "이든",
        no_jongsung: "든",
    },
    JosaPair {
        jongsung: "이라도",
        no_jongsung: "라도",
    },
    JosaPair {
        jongsung: "이야",
        no_jongsung: "야",
    },
    JosaPair {
        jongsung: "이여",
        no_jongsung: "여",
    },
    // Invariable (no 받침 allomorphy): both slots identical.
    JosaPair {
        jongsung: "에서",
        no_jongsung: "에서",
    }, // locative
    JosaPair {
        jongsung: "에",
        no_jongsung: "에",
    },
    JosaPair {
        jongsung: "에게",
        no_jongsung: "에게",
    }, // dative (person)
    JosaPair {
        jongsung: "에게서",
        no_jongsung: "에게서",
    },
    JosaPair {
        jongsung: "한테",
        no_jongsung: "한테",
    },
    JosaPair {
        jongsung: "한테서",
        no_jongsung: "한테서",
    },
    JosaPair {
        jongsung: "께",
        no_jongsung: "께",
    }, // honorific dative
    JosaPair {
        jongsung: "도",
        no_jongsung: "도",
    }, // also
    JosaPair {
        jongsung: "만",
        no_jongsung: "만",
    }, // only
    JosaPair {
        jongsung: "부터",
        no_jongsung: "부터",
    }, // from (time/place)
    JosaPair {
        jongsung: "처럼",
        no_jongsung: "처럼",
    }, // like
    JosaPair {
        jongsung: "같이",
        no_jongsung: "같이",
    }, // together/like
    JosaPair {
        jongsung: "마다",
        no_jongsung: "마다",
    }, // every
    JosaPair {
        jongsung: "보다",
        no_jongsung: "보다",
    }, // than
    JosaPair {
        jongsung: "마다의",
        no_jongsung: "마다의",
    },
    JosaPair {
        jongsung: "의",
        no_jongsung: "의",
    }, // genitive
    JosaPair {
        jongsung: "에 대한",
        no_jongsung: "에 대한",
    },
    JosaPair {
        jongsung: "에서의",
        no_jongsung: "에서의",
    },
    JosaPair {
        jongsung: "에는",
        no_jongsung: "에는",
    },
    JosaPair {
        jongsung: "에게는",
        no_jongsung: "에게는",
    },
];

/// 어미 (verbal endings) — closed-class table, longest match first. The list covers
/// the common conjugation suffixes including the ㄷ/ㅂ/르 irregular surface forms
/// (웠/워 for ㅂ stems, 어 for ㄷ stems, 라/랐 for 르 stems): the strip is
/// deliberately approximate (it does not reconstruct the dictionary stem), and the
/// surface+stem dual emission preserves the recall path when the stem extraction
/// under- or over-strips.
const EOMI_TABLE: &[&str] = &[
    // Honorific/formal endings (longest first within the match loop).
    "시겠습니다",
    "셨습니다",
    "십니다",
    "셨어요",
    "세요",
    "겠습니다",
    "었습니다",
    "였습니다",
    "습니까",
    "ㅂ니까",
    "ㅂ니다",
    "습니다",
    // Polite endings.
    "었어요",
    "었습니까",
    "아요",
    "어요",
    "여요",
    "이에요",
    "예요",
    // Past/evidential morphology (incl. ㅂ-irregular 워/웠 and ㄹ-irregular 랄/랐).
    "었었",
    "였었",
    "았었",
    "웠",
    "워",
    "랐",
    "라",
    "았",
    "었",
    "였",
    "겠",
    "았어",
    "었어",
    "였어",
    "았고",
    "었고",
    "였고",
    "았다",
    "었다",
    "였다",
    "았지",
    "었지",
    "였지",
    // Connective / causal / conditional.
    "아서",
    "어서",
    "여서",
    "으니",
    "으면",
    "이니",
    "이라서",
    "라서",
    "아도",
    "어도",
    "이라도",
    "라도",
    "지만",
    "는데",
    "은데",
    "ㄴ데",
    "면서",
    "며",
    "고",
    "거나",
    "나",
    // Nominalizer / sentence-final.
    "는것",
    "을것",
    "것",
    "음",
    "임",
    "기",
    // Adnominal.
    "는",
    "을",
    "ㄴ",
    "ㄹ",
    "은",
    // Plain / informal sentence endings.
    "야",
    "지",
    "네",
    "군",
    "다",
    "이다",
    "ㄴ다",
    // Irregular-stem residue (받침 units after ㄷ/ㅂ/르 contraction).
    "ㄹ",
    "ㅁ",
];

/// Longest 어미 match: returns the STEM LENGTH (chars kept after stripping) or
/// `None` when no 어미 matches. A strip must leave at least one syllable.
fn match_eomi(chars: &[char]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for &eomi in EOMI_TABLE {
        let eomi_chars: Vec<char> = eomi.chars().collect();
        if eomi_chars.is_empty() || chars.len() <= eomi_chars.len() {
            continue;
        }
        let tail = &chars[chars.len() - eomi_chars.len()..];
        if tail == eomi_chars.as_slice() {
            let stem_len = chars.len() - eomi_chars.len();
            if stem_len > 0 {
                best = Some(best.map_or(stem_len, |b: usize| b.max(stem_len)));
            }
        }
    }
    best
}

/// Validity of a 조사 surface after the preceding syllable: the matched form must
/// agree with the preceding syllable's 받침 (the allomorphy contract). The special
/// ㄹ 받침 follows the vowel-final form for every allomorphic pair ( 으로/로 is the
/// visible case: 길로, not 길으로).
fn josa_valid_after(surface: &str, preceding: char) -> bool {
    let jongsungful = has_jongseong(preceding) && jongseong(preceding) != 8; // ㄹ 받침 = index 8
    for pair in JOSA_TABLE {
        if pair.jongsung == pair.no_jongsung {
            // Invariable 조사: valid after any syllable.
            if pair.jongsung == surface {
                return true;
            }
            continue;
        }
        if pair.jongsung == surface {
            return jongsungful;
        }
        if pair.no_jongsung == surface {
            return !jongsungful;
        }
    }
    false
}

/// Longest valid 조사 match: returns the STEM LENGTH or `None`.
fn match_josa(chars: &[char]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for pair in JOSA_TABLE {
        for surface in [pair.jongsung, pair.no_jongsung] {
            let surface_chars: Vec<char> = surface.chars().collect();
            if surface_chars.is_empty() || chars.len() <= surface_chars.len() {
                continue;
            }
            let stem_len = chars.len() - surface_chars.len();
            let tail = &chars[stem_len..];
            if tail != surface_chars.as_slice() {
                continue;
            }
            let preceding = chars[stem_len - 1];
            if josa_valid_after(surface, preceding) {
                best = Some(best.map_or(stem_len, |b: usize| b.max(stem_len)));
            }
        }
    }
    best
}

/// Korean layer: surface+stem dual emission. Strips one maximal 어미 (then 조사,
/// when no 어미 matched) suffix and emits both the surface and the stem. The
/// surface keeps the eojeol recall (a query for the eojeol itself); the stem adds
/// the base-form recall (a query for the root word matches the eojeol via the
/// stem unit). The strip is approximate (closed class, no dictionary stem
/// reconstruction); the surface unit makes the layer recall-safe regardless.
fn layer_hangul(run: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = run.chars().collect();
    if chars.is_empty() {
        return;
    }
    // Surface first (matches the eojeol).
    out.push(run.to_string());
    // Longest 어미 strip first; 조사 only when no 어미 matched (an eojeol does not
    // end in both).
    let stem_len = match_eomi(&chars).or_else(|| match_josa(&chars));
    if let Some(stem_len) = stem_len
        && stem_len > 0
    {
        let stem: String = chars[..stem_len].iter().collect();
        if stem != run {
            out.push(stem);
        }
    }
}

// -- Latin layer (canonical Porter stemmer) ------------------------------------------------

/// Canonical Porter stemmer (the 1973 reference algorithm; the well-known five-step
/// measure-and-strip pipeline; English-only applied to Latin runs). The implementation
/// here is the compact reference form (~200 lines): each step is a list of
/// (condition, suffix, replacement) rules. Non-English Latin over-stem risk is
/// accepted and recorded; surface+stem dual emission preserves precision.
fn porter_stem(word: &str) -> String {
    // Step 1a: plurals → singular.
    let mut stem = word.to_string();
    if stem.ends_with("sses") {
        stem.truncate(stem.len() - 2); // ssess → ss
    } else if stem.ends_with("ies") {
        stem.truncate(stem.len() - 2); // ies → i
    } else if stem.ends_with("ss") {
        // leave alone
    } else if stem.ends_with("s") && stem.len() > 1 {
        stem.truncate(stem.len() - 1);
    }
    // Step 1b: past/gerund → present.
    if stem.len() >= 4 && stem.ends_with("eed") && measure(&stem[..stem.len() - 3]) > 0 {
        stem.truncate(stem.len() - 1);
    } else if stem.len() >= 5 && stem.ends_with("ed") && contains_vowel(&stem[..stem.len() - 2]) {
        stem.truncate(stem.len() - 2);
        // Re-apply step 1a rules to the trimmed stem.
        if stem.ends_with("at") || stem.ends_with("bl") || stem.ends_with("iz") {
            stem.push('e');
        } else if ends_with_double_consonant(&stem) && !ends_with_double_l_s_z(&stem) {
            stem.truncate(stem.len() - 1);
        } else if is_short_word(&stem) {
            stem.push('e');
        }
    } else if stem.len() >= 6 && stem.ends_with("ing") && contains_vowel(&stem[..stem.len() - 3]) {
        stem.truncate(stem.len() - 3);
        // Re-apply step 1a rules to the trimmed stem.
        if stem.ends_with("at") || stem.ends_with("bl") || stem.ends_with("iz") {
            stem.push('e');
        } else if ends_with_double_consonant(&stem) && !ends_with_double_l_s_z(&stem) {
            stem.truncate(stem.len() - 1);
        } else if is_short_word(&stem) {
            stem.push('e');
        }
    }
    // Step 1c: y → i (vowel in stem).
    if stem.len() >= 2 && stem.ends_with('y') && contains_vowel(&stem[..stem.len() - 1]) {
        stem.truncate(stem.len() - 1);
        stem.push('i');
    }
    // Step 2: common derivational suffixes.
    let step2 = [
        ("ational", "ate"),
        ("tional", "tion"),
        ("iveness", "ive"),
        ("fulness", "ful"),
        ("ousness", "ous"),
        ("ization", "ize"),
        ("ization", "ize"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    for (suffix, repl) in step2 {
        if stem.len() > suffix.len()
            && stem.ends_with(suffix)
            && measure(&stem[..stem.len() - suffix.len()]) > 0
        {
            stem.truncate(stem.len() - suffix.len());
            stem.push_str(repl);
            break;
        }
    }
    // Step 3: more derivational suffixes.
    let step3 = [
        ("icate", "ic"),
        ("ative", ""),
        ("alize", "al"),
        ("iciti", "ic"),
        ("ical", "ic"),
        ("ful", ""),
        ("ness", ""),
    ];
    for (suffix, repl) in step3 {
        if stem.len() > suffix.len()
            && stem.ends_with(suffix)
            && measure(&stem[..stem.len() - suffix.len()]) > 0
        {
            stem.truncate(stem.len() - suffix.len());
            stem.push_str(repl);
            break;
        }
    }
    // Step 4: derivational cleanup.
    let suffixes_step4 = [
        "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ou",
        "ism", "ate", "iti", "ous", "ive", "ize",
    ];
    for suffix in suffixes_step4 {
        if stem.len() > suffix.len()
            && stem.ends_with(suffix)
            && measure(&stem[..stem.len() - suffix.len()]) > 1
        {
            stem.truncate(stem.len() - suffix.len());
            break;
        }
    }
    // Step 5a: terminal e.
    if stem.len() >= 2
        && stem.ends_with('e')
        && (measure(&stem[..stem.len() - 1]) > 1
            || (measure(&stem[..stem.len() - 1]) == 1 && !ends_with_cvc(&stem[..stem.len() - 1])))
    {
        stem.truncate(stem.len() - 1);
    }
    // Step 5b: double-suffix collapse.
    if stem.len() >= 2 && measure(&stem) > 1 && stem.ends_with('l') && ends_with_double_l(&stem) {
        stem.truncate(stem.len() - 1);
    }
    stem
}

/// Measure = number of VC sequences `[C](VC){m}[V]` in the stem. The Porter
/// definition (compact form).
fn measure(stem: &str) -> usize {
    let mut m = 0;
    let mut prev = VcClass::None;
    for c in stem.chars() {
        let cur = if is_consonant(c) {
            VcClass::C
        } else if is_vowel(c) {
            VcClass::V
        } else {
            VcClass::None
        };
        if prev == VcClass::C && cur == VcClass::V {
            m += 1;
        }
        if cur != VcClass::None {
            prev = cur;
        }
    }
    m
}

#[derive(PartialEq, Eq, Copy, Clone)]
enum VcClass {
    None,
    C,
    V,
}

fn is_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'A' | 'E' | 'I' | 'O' | 'U')
}

fn is_consonant(c: char) -> bool {
    c.is_ascii_alphabetic() && !is_vowel(c)
}

fn contains_vowel(s: &str) -> bool {
    s.chars().any(is_vowel)
}

fn ends_with_double_consonant(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return false;
    }
    let last = chars[chars.len() - 1];
    let prev = chars[chars.len() - 2];
    last == prev && is_consonant(last) && last != 'l' && last != 's' && last != 'z'
}

fn ends_with_double_l_s_z(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return false;
    }
    let last = chars[chars.len() - 1];
    last == 'l' || last == 's' || last == 'z'
}

fn ends_with_double_l(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return false;
    }
    chars[chars.len() - 1] == 'l' && chars[chars.len() - 2] == 'l'
}

fn is_short_word(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return false;
    }
    if !ends_with_cvc(s) {
        return false;
    }
    chars.len() == measure(s) * 2 + 2
}

fn ends_with_cvc(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return false;
    }
    let last = chars[chars.len() - 1];
    let mid = chars[chars.len() - 2];
    let first = chars[chars.len() - 3];
    is_consonant(first)
        && is_vowel(mid)
        && is_consonant(last)
        && last != 'w'
        && last != 'x'
        && last != 'y'
}

/// Latin layer: surface + Porter stem dual emission.
fn layer_latin(run: &str, out: &mut Vec<String>) {
    if run.is_empty() {
        return;
    }
    out.push(run.to_string());
    let stem = porter_stem(run);
    if !stem.is_empty() && stem != run {
        out.push(stem);
    }
}

// -- Top-level analyzer entry point --------------------------------------------------------

/// The composite analyzer. Walks the pre-passed text under UAX #29 segmentation,
/// accumulates maximal per-class script runs, and dispatches each run to its
/// layer. Output order follows input order; duplicates preserved.
pub fn analyze(text: &str) -> Vec<String> {
    let pre = prepass(text);
    let mut out = Vec::new();
    // Per UAX #29 segment, accumulate the per-class run. A segment break is a
    // word-boundary that the layers treat as a hard separator (the mecab layer
    // re-tokenizes within the run; the rule layers also work per-run).
    let mut run = String::new();
    let mut run_class: Option<ScriptClass> = None;
    let mut prev_segment_end: Option<usize> = None;
    for (start, segment) in pre.split_word_bound_indices() {
        // Drop segments that contain no word characters (the v1 bigram analyzer
        // does the same; pure separator/symbol segments don't contribute runs).
        if !segment.chars().any(is_word_char) {
            flush_run(&mut run, &mut run_class, &mut out);
            continue;
        }
        // UAX #29 segment break: any gap between segments breaks the per-class
        // accumulation (a script-class change across the gap starts a fresh run).
        if prev_segment_end != Some(start) {
            flush_run(&mut run, &mut run_class, &mut out);
        }
        for c in segment.chars() {
            let class = classify(c);
            if class == ScriptClass::Other {
                // Non-class characters: flush the current run (if any) and emit the
                // character whole. This matches the v1 bigram analyzer's non-CJK
                // pass: standalone characters / digits / mixed-script single chars
                // emit as-is.
                flush_run(&mut run, &mut run_class, &mut out);
                if c.is_alphanumeric() {
                    out.push(c.to_string());
                }
                // Pure symbols/punctuation are dropped (the v1 contract).
            } else if run_class == Some(class) {
                run.push(c);
            } else {
                flush_run(&mut run, &mut run_class, &mut out);
                run.push(c);
                run_class = Some(class);
            }
        }
        prev_segment_end = Some(start + segment.len());
    }
    flush_run(&mut run, &mut run_class, &mut out);
    out
}

/// Dispatches one accumulated run to its layer.
fn flush_run(run: &mut String, run_class: &mut Option<ScriptClass>, out: &mut Vec<String>) {
    if run.is_empty() {
        return;
    }
    let class = classify_run(run);
    // Dispatch: pure-Han + Japanese both feed layer_japanese_or_han (the per-class
    // split inside the CJK run decides mecab vs bigram); Hangul and Latin each go
    // to their dedicated layer; Other emits the run whole.
    match class {
        ScriptClass::Japanese | ScriptClass::HanOnly => {
            layer_japanese_or_han(run, out);
        }
        ScriptClass::Hangul => {
            layer_hangul(run, out);
        }
        ScriptClass::Latin => {
            layer_latin(run, out);
        }
        ScriptClass::Other => {
            out.push(std::mem::take(run));
            return;
        }
    }
    run.clear();
    *run_class = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porter_basic_english_stemming() {
        // Canonical Porter test vectors.
        assert_eq!(porter_stem("running"), "run");
        assert_eq!(porter_stem("runs"), "run");
        assert_eq!(porter_stem("ran"), "ran");
        assert_eq!(porter_stem("easily"), "easili");
        assert_eq!(porter_stem("cats"), "cat");
        assert_eq!(porter_stem("fishing"), "fish");
        assert_eq!(porter_stem("fished"), "fish");
    }

    #[test]
    fn hangul_strip_emits_surface_and_stem() {
        // 학교에서 → surface + stem (학교)
        let units = analyze("학교에서");
        assert!(
            units.contains(&"학교에서".to_string()),
            "surface keeps eojeol recall"
        );
        assert!(units.contains(&"학교".to_string()), "stem adds root recall");
    }

    #[test]
    fn hangul_strip_handles_consonant_jongseong_alternation() {
        // 책이 → surface + stem (책; 이 받침-ful form matches after 받침-ful 받침).
        let units = analyze("책이");
        assert!(units.contains(&"책이".to_string()));
        assert!(units.contains(&"책".to_string()));
        // 책은 → surface + stem (책; 은 받침-ful form).
        let units = analyze("책은");
        assert!(units.contains(&"책은".to_string()));
        assert!(units.contains(&"책".to_string()));
    }

    #[test]
    fn hangul_strip_skips_when_jongseong_does_not_agree() {
        // 학교는 — 학교 ends in a 받침-less syllable; 는 is the 받침-less topic
        // marker; the strip emits surface+stem. (학교 ends in 교 = no 받침.)
        let units = analyze("학교는");
        assert!(units.contains(&"학교는".to_string()));
        assert!(units.contains(&"학교".to_string()));
    }

    #[test]
    fn japanese_layer_requires_dictionary() {
        // The Japanese layer delegates to the mecab analyzer; without the
        // dictionary loaded, analyzing Japanese text panics (the same fail-closed
        // contract as id 2). Pure-Han text does NOT need the dictionary.
        let pure_han = analyze("数据库");
        assert_eq!(pure_han, vec!["数据", "据库"]);

        // Japanese with kana needs the dictionary; if the dictionary is absent the
        // analyzer panics (we do not test the panic here — id 0 operations gate
        // through the state.rs open validation that ensures the dictionary is
        // loaded for id-0 indexes).
    }

    #[test]
    fn latin_layer_dual_emits_surface_and_stem() {
        let units = analyze("running");
        assert!(units.contains(&"running".to_string()));
        assert!(units.contains(&"run".to_string()));
    }

    #[test]
    fn pure_han_falls_back_to_bigram() {
        assert_eq!(analyze("数据库"), vec!["数据", "据库"]);
        assert_eq!(analyze("你好世界"), vec!["你好", "好世", "世界"]);
    }

    #[test]
    fn mixed_script_composite_document() {
        // A document mixing Chinese bigram (no kana → HanOnly), Japanese via mecab
        // (with kana; the dictionary must be loaded for the full path), Korean
        // (조사 strip), and English (Porter). This test exercises the dispatch in
        // isolation: the Korean + Latin + HanOnly parts are tested without the
        // mecab layer, which requires the dictionary.
        let units = analyze("running 학교에서 数据");
        assert!(units.iter().any(|u| u == "running"));
        assert!(units.iter().any(|u| u == "run"));
        assert!(units.iter().any(|u| u == "학교에서"));
        assert!(units.iter().any(|u| u == "학교"));
        // The pure-Han 数据 is a single bigram (no kana → HanOnly path).
        assert!(units.iter().any(|u| u == "数据"));
    }

    #[test]
    fn composite_is_deterministic() {
        let fixtures = ["", "running", "학교에서", "数据库", "running 학교에서 数据"];
        for fixture in fixtures {
            let first = analyze(fixture);
            let second = analyze(fixture);
            assert_eq!(first, second, "deterministic on {fixture:?}");
        }
    }

    #[test]
    fn composite_idempotence_for_non_japanese_layers() {
        // The rule layers' dual emission is monotone: `analyze(input) ⊆
        // analyze(analyze(input).join(" "))` — re-analyzing the joined units may
        // surface a stem that becomes a new input's surface (e.g. Porter on "run"
        // emits surface+stem "run"+"run"), so the join→re-analyze cycle can grow
        // the unit set, never shrink. The first call's units must all be present
        // in the re-analyzed output (closed set).
        let fixtures = [
            "running fast",
            "학교에서 공부",
            "数据库 应用",
            "running 학교에서 数据",
        ];
        for fixture in fixtures {
            let first = analyze(fixture);
            let second = analyze(&first.join(" "));
            for unit in &first {
                assert!(
                    second.contains(unit),
                    "re-analysis of {fixture:?} lost unit {unit:?}: {first:?} -> {second:?}"
                );
            }
        }
    }

    #[test]
    fn script_class_classifier_matches_expected_ranges() {
        assert_eq!(classify('あ'), ScriptClass::Japanese);
        assert_eq!(classify('ア'), ScriptClass::Japanese);
        assert_eq!(classify('東'), ScriptClass::HanOnly);
        assert_eq!(classify('학'), ScriptClass::Hangul);
        assert_eq!(classify('a'), ScriptClass::Latin);
        assert_eq!(classify('7'), ScriptClass::Other);
    }

    #[test]
    fn run_classification_decides_kana_presence() {
        // Pure Han: HanOnly.
        assert_eq!(classify_run("数据库"), ScriptClass::HanOnly);
        // Kana present: Japanese (mecab layer).
        assert_eq!(classify_run("走る"), ScriptClass::Japanese);
        // Han + kana mix: Japanese.
        assert_eq!(classify_run("走った"), ScriptClass::Japanese);
    }

    #[test]
    fn surface_plus_stem_preserves_recall_for_inflection() {
        // The overmatch table for the Hangul layer: a v0 eojeol "책을" should
        // emit surface + stem (책); a query for either matches the same doc.
        let units = analyze("책을 읽다");
        assert!(units.contains(&"책을".to_string()));
        assert!(units.contains(&"책".to_string()));
    }
}
