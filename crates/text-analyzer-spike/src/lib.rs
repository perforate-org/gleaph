//! Plan 0330 scratch spike crate: native measurement harness for the ANALYZER_ID=2
//! Japanese lemmatization analyzer candidates. NOT wired into any canister or the
//! pocket-ic build; this crate exists to record measurements in plan 0330 and is
//! deleted (or parked) once plan 0331 lands the winner.
//!
//! Candidates (feature-gated, each exposing the same `fn analyze(text: &str) -> Vec<String>`
//! over the SHARED pre-pass module `prepass`, which mirrors
//! `crates/text-canister/src/analyzer.rs` (NFKC + Unicode lowercase) so candidates differ
//! only in the segmentation/lemma stage):
//!
//! - `rule` (A): dictionary-free rule-based stemmer — UAX #29 segmentation +
//!   katakana→hiragana folding + a conjugation-suffix FSA emitting stem candidates for
//!   inflectable words, CJK bigrams (v1 parity) otherwise.
//! - `vibrato` (B): vibrato 0.5.2 + ipadic-mecab compiled dictionary loaded from bytes.
//! - `sudachi` (C): sudachi.rs 0.6.11 (suiko-sudachi crates.io redistribution) +
//!   SudachiDict small loaded from bytes.
//! - `lindera` (D): lindera 6.0 + ipadic embedded dictionary (bytes-loading of the system
//!   dictionary does not exist in lindera 6.0 — `Dictionary::load_from_path` is
//!   path-only; recorded in plan 0330 as "D is wasm-embedded-only").
//!
//! All measurements (wasm32 size, heap, throughput, recall/overmatch) are recorded in
//! `plans/0330-text-analyzer-spike.md` — see `tests/fixtures.rs` (recall/determinism/
//! idempotence/overmatch) and `tests/measure.rs` (heap + throughput + wasm sizes printed
//! from `build_wasm.sh` runs).

pub mod harness;
#[cfg(feature = "lindera")]
pub mod lindera_candidate;
#[cfg(feature = "rule")]
pub mod rule;
#[cfg(feature = "sudachi")]
pub mod sudachi_candidate;
#[cfg(feature = "vibrato")]
pub mod vibrato_candidate;

/// Shared pre-pass mirroring `crates/text-canister/src/analyzer.rs`'s normalization
/// order (NFKC + Unicode lowercase). Applied to the WHOLE text (the v1 analyzer applies
/// it per UAX #29 segment; whole-text NFKC is equivalent for the spike and preserves
/// word adjacency so dictionary analyzers see natural text).
pub fn prepass(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    text.nfkc().collect::<String>().to_lowercase()
}

/// True for characters eligible for CJK-run bigram expansion (same classes as v1
/// analyzer.rs): Hiragana U+3041..=U+3096, Katakana U+30A1..=U+30FF, and CJK Unified
/// Ideographs U+4E00..=U+9FFF.
#[cfg_attr(not(feature = "rule"), allow(dead_code))]
pub(crate) fn is_cjk_char(c: char) -> bool {
    matches!(
        c,
        '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}'
    )
}

/// Fold a katakana character to hiragana (v1-parity 表記ゆれ aid for the rule candidate).
#[cfg_attr(not(feature = "rule"), allow(dead_code))]
pub(crate) fn katakana_to_hiragana(c: char) -> char {
    if ('\u{30A1}'..='\u{30F6}').contains(&c) {
        char::from_u32(c as u32 - 0x60).unwrap_or(c)
    } else {
        c
    }
}
/// wasm32 size probe: an exported C symbol that runs every compiled-in candidate over
/// a short input and sums the unit counts. Without this export the cdylib link step
/// dead-code-eliminates the entire analyzer (including any embedded dictionary data),
/// making wasm size accounting meaningless — `build_wasm.sh` measures THIS artifact.
/// # Safety
///
/// `ptr` must point to `len` valid UTF-8 bytes for the duration of the call.
#[allow(unused_mut, dead_code)] // mut/used-ness is feature-dependent (cfg arms below)
#[unsafe(no_mangle)]
pub unsafe extern "C" fn spike_probe(ptr: *const u8, len: usize) -> usize {
    let text = match std::str::from_utf8(unsafe { std::slice::from_raw_parts(ptr, len) }) {
        Ok(t) => t,
        Err(_) => return 0,
    };
    let mut total = 0usize;
    #[cfg(feature = "rule")]
    {
        total += rule::analyze(text).len();
    }
    // Dictionary-backed candidates are probed through their wasm-shape entry points
    // (embedded://ipadic for D); the harness file loaders are native-only.
    #[cfg(feature = "lindera")]
    {
        if let Ok(a) = lindera_candidate::Analyzer::embedded() {
            total += a.analyze(text).len();
        }
    }
    let _ = text;
    total
}
