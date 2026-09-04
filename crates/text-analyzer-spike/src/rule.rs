//! Candidate A — dictionary-free rule-based stemmer.
//!
//! Pipeline over the shared pre-pass output (NFKC + lowercase):
//!
//! 1. split the text into maximal contiguous CJK runs (Han + Hiragana + Katakana, the
//!    same character classes as the v1 bigram analyzer) and non-CJK alnum words;
//! 2. CJK runs keep the v1-parity overlapping-bigram expansion (a lone CJK char stays a
//!    unigram);
//! 3. a run whose tail is a conjugated KANA suffix (五段/一段/サ変/カ変 verb +
//!    形容詞 i-adjective tables) ALSO emits its kanji-only STEM prefix — 走った and
//!    走る both share the stem unit 走 without any dictionary. The kanji-prefix rule
//!    sidesteps the 五段 past-tense ambiguity (買った: 買う/買つ) by sharing the stem,
//!    and it avoids stripping from noun runs (which end in kanji, not kana).
//!
//! Katakana is folded to hiragana before suffix matching. Output order follows input
//! order; duplicates preserved.

use crate::{is_cjk_char, katakana_to_hiragana, prepass};

/// Conjugated kana suffixes recognized by the FSA (longest match first).
const KANA_SUFFIXES: &[&str] = &[
    "なかった",
    "ませんでした",
    "ません",
    "ました",
    "ます",
    "って",
    "った",
    "たら",
    "たり",
    "れば",
    "れる",
    "られ",
    "せる",
    "させ",
    "たい",
    "なかった",
    "ない",
    "て",
    "た",
    "ろう",
    "よう",
    "ん",
    "ぬ",
    "る",
    "う",
    "い",
    "くて",
    "く",
];

/// Analyze `text` into indexable units (input: raw text; the pre-pass is applied here).
pub fn analyze(text: &str) -> Vec<String> {
    let normalized = prepass(text);
    let mut out = Vec::new();
    for piece in split_cjk_runs(&normalized) {
        match piece {
            Piece::CjkRun(run) => analyze_cjk_run(&run, &mut out),
            Piece::Word(word) => out.push(word),
        }
    }
    out
}

enum Piece {
    /// Maximal run of contiguous CJK characters.
    CjkRun(String),
    /// Non-CJK alphanumeric word (whole unit).
    Word(String),
}

/// Split the normalized text into CJK runs and non-CJK words, mirroring the v1
/// analyzer's CJK-run accumulation across adjacent segments (any intervening
/// separator breaks the run).
fn split_cjk_runs(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut run = String::new();
    let mut word = String::new();
    for c in text.chars() {
        if is_cjk_char(c) {
            if !word.is_empty() {
                let word = std::mem::take(&mut word);
                if word.chars().any(|c| c.is_alphanumeric()) {
                    pieces.push(Piece::Word(word));
                }
            }
            run.push(c);
        } else {
            if !run.is_empty() {
                pieces.push(Piece::CjkRun(std::mem::take(&mut run)));
            }
            if !c.is_whitespace() {
                word.push(c);
            } else if !word.is_empty() {
                let word = std::mem::take(&mut word);
                if word.chars().any(|c| c.is_alphanumeric()) {
                    pieces.push(Piece::Word(word));
                }
            }
        }
    }
    if !run.is_empty() {
        pieces.push(Piece::CjkRun(run));
    }
    if !word.is_empty() && word.chars().any(|c| c.is_alphanumeric()) {
        pieces.push(Piece::Word(word));
    }
    pieces
}

fn analyze_cjk_run(run: &str, out: &mut Vec<String>) {
    emit_bigrams(run, out);
    let folded: String = run.chars().map(katakana_to_hiragana).collect();
    if let Some(stem) = stem_of(&folded) {
        out.push(stem);
    }
}

/// v1-parity CJK-run expansion: overlapping bigrams, a lone CJK character stays a
/// unigram.
fn emit_bigrams(run: &str, out: &mut Vec<String>) {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() == 1 {
        out.push(run.to_string());
    } else {
        for window in chars.windows(2) {
            out.push(window.iter().collect());
        }
    }
}

/// Conjugation-suffix FSA: when the run ends with a conjugated KANA tail that matches
/// a suffix (longest match), the immediately preceding kanji block is emitted as the
/// stem. 走った and 走る both share 走; the kanji-prefix rule sidesteps the 五段
/// past-tense ambiguity (買った: 買う/買つ) and never strips from kanji-final runs
/// (走行, 附属) — the overmatch fixture table records the residual rate.
fn stem_of(run: &str) -> Option<String> {
    let chars: Vec<char> = run.chars().collect();
    if chars.last().is_none_or(|c| !is_cjk_kana(*c)) {
        return None; // kanji-final run: no conjugation tail
    }
    let kana_start = chars
        .iter()
        .rposition(|&c| !is_cjk_kana(c))
        .map_or(0, |i| i + 1);
    // kanji block immediately preceding the kana tail
    let kanji_end = kana_start;
    let kanji_start = chars[..kanji_end]
        .iter()
        .rposition(|&c| is_cjk_kana(c))
        .map_or(0, |i| i + 1);
    let kanji_block: String = chars[kanji_start..kanji_end].iter().collect();
    if kanji_block.is_empty() {
        return None;
    }
    let kana_tail: String = chars[kana_start..].iter().collect();
    for suffix in KANA_SUFFIXES {
        if kana_tail.ends_with(suffix) {
            return Some(kanji_block);
        }
    }
    None
}

fn is_cjk_kana(c: char) -> bool {
    ('\u{3041}'..='\u{3096}').contains(&c) || ('\u{30A1}'..='\u{30FF}').contains(&c)
}
