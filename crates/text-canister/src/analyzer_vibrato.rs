//! ANALYZER_ID=2 pipeline: vibrato 0.5.2 + ipadic-mecab lemma units (plan 0330 decision B,
//! plan 0331 landing). Ported from the plan 0330 spike's `vibrato_candidate` module.
//!
//! Pipeline: whole-text NFKC + Unicode lowercase pre-pass (the v1 analyzer's normalization
//! order, applied to the whole text so word adjacency survives for the dictionary), then
//! per-line vibrato tokenization over the stable-resident ipadic dictionary, emitting the
//! ipadic base form (基本形, feature column 6) for content words. 助詞/助動詞/記号/接頭辞/
//! 接尾辞/フィラー tokens are dropped; a token whose base form is the unassigned marker `*`
//! emits its surface instead (proper nouns, uncategorized forms). Output is a deterministic
//! function of the input and strict idempotence holds:
//! `analyze(units.join(" ")) == analyze(input)` for every fixture (whitespace surface
//! tokens are skipped, so re-analyzing the space-joined units reproduces the units).
//!
//! Dictionary lifecycle: the ZSTD artifact bytes live in stable region 16 (plan 0331);
//! `load_dictionary` receives the DECOMPRESSED raw bytes (ruzstd decode happens in the
//! caller) and builds the pinned [`Tokenizer`] once. The tokenizer is process-resident
//! heap state (~52 MB) rebuilt at init/upgrade/finalize — never lazily per query call,
//! so the 5B query-call instruction budget never pays dictionary construction.

use std::cell::RefCell;
#[cfg(test)]
use std::io::Read;

use unicode_normalization::UnicodeNormalization;
use vibrato::{Dictionary, Tokenizer};

thread_local! {
    /// The pinned vibrato tokenizer over the finalized ipadic dictionary. `None` until
    /// the stable dictionary is finalized (and loaded); analyzer 2 operations fail
    /// closed before that point.
    static TOKENIZER: RefCell<Option<Tokenizer>> = const { RefCell::new(None) };
}

/// Loads the pinned tokenizer from RAW (decompressed) system dictionary bytes. Called
/// from the init/open path (finalized region 16) and from `admin_finalize_dict_upload`.
/// Fails closed on a corrupt artifact — the caller must not record Finalized state for
/// bytes that cannot decode.
pub fn load_dictionary(raw_dict: &[u8]) -> Result<(), String> {
    let dict = Dictionary::read(raw_dict).map_err(|error| {
        format!("ipadic dictionary load failed (corrupt or truncated artifact): {error}")
    })?;
    let tokenizer = Tokenizer::new(dict);
    TOKENIZER.with(|slot| {
        *slot.borrow_mut() = Some(tokenizer);
    });
    Ok(())
}

/// Whether the pinned dictionary tokenizer is resident. Open-time validation guarantees
/// it is loaded exactly when meta pins analyzer 2 AND the dictionary is finalized, so
/// this is diagnostic-only (tests + the state open path).
#[cfg_attr(not(test), allow(dead_code))]
pub fn dictionary_loaded() -> bool {
    TOKENIZER.with(|slot| slot.borrow().is_some())
}

/// Drops the resident tokenizer (test helper; production reloads only at finalize/open).
#[cfg(test)]
#[allow(dead_code)]
pub fn reset() {
    TOKENIZER.with(|slot| *slot.borrow_mut() = None);
}

/// Analyzes `text` into ipadic lemma units. Panics fail-closed when the dictionary is
/// not resident — callers gate every analyze-touching operation on the finalized
/// dictionary state (open validation + ingest/search preflight), so a missing tokenizer
/// here is a broken invariant, never an expected condition.
pub fn analyze(text: &str) -> Vec<String> {
    let tokenizer_held = TOKENIZER.with(|slot| slot.borrow().is_some());
    assert!(
        tokenizer_held,
        "analyzer 2 (vibrato) used without a finalized ipadic dictionary — \
         broken open/finalize invariant"
    );
    TOKENIZER.with(|slot| {
        let tokenizer = slot.borrow();
        let tokenizer = tokenizer.as_ref().expect("checked above");
        analyze_with(tokenizer, text)
    })
}

/// Tokenizes pre-normalized text with `tokenizer` (unit-testable core).
fn analyze_with(tokenizer: &Tokenizer, text: &str) -> Vec<String> {
    // Whole-text NFKC + lowercase, matching the v1 pipeline's normalization order.
    let normalized = text.nfkc().collect::<String>().to_lowercase();
    let mut out = Vec::new();
    for line in normalized.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut worker = tokenizer.new_worker();
        worker.reset_sentence(line);
        worker.tokenize();
        for token in worker.token_iter() {
            if token.surface().trim().is_empty() {
                continue; // whitespace token (from the joined-unit re-analysis)
            }
            let columns: Vec<&str> = token.feature().split(',').collect();
            let pos = columns.first().copied().unwrap_or("*");
            if matches!(
                pos,
                "助詞" | "助動詞" | "記号" | "接頭辞" | "接尾辞" | "フィラー"
            ) {
                continue;
            }
            let base = columns.get(6).copied().unwrap_or("*");
            if base == "*" || base.is_empty() {
                out.push(token.surface().to_string());
            } else {
                out.push(base.to_string());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decompressed ipadic dictionary path (gitignored; see
    /// `crates/pocket-ic-tests/resources/` and the plan 0331 E2E fetch helper).
    /// Tests needing the dictionary SKIP (loudly) when the artifact is absent so the
    /// suite stays runnable without the 8 MB download; the E2E fetches it fail-closed.
    fn with_dictionary_if_present() -> Option<()> {
        let compressed = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pocket-ic-tests/resources/vibrato/system.dic.zst"
        ))
        .ok()?;
        let mut decoder = ruzstd::StreamingDecoder::new(&compressed[..]).ok()?;
        let mut raw = Vec::with_capacity(compressed.len() * 3);
        decoder.read_to_end(&mut raw).ok()?;
        load_dictionary(&raw).ok()?;
        Some(())
    }

    #[test]
    fn load_rejects_corrupt_bytes_without_residency() {
        assert!(load_dictionary(b"not a dictionary").is_err());
        assert!(!dictionary_loaded());
    }

    #[test]
    fn vibrato_unit_emission_fixtures() {
        let Some(()) = with_dictionary_if_present() else {
            eprintln!("SKIPPED (ipadic dictionary not fetched; see pocket-ic-tests resources)");
            return;
        };
        // Lemma recall: 走った emits the base form 走る; the auxiliary た is dropped.
        assert_eq!(analyze("走った"), vec!["走る"]);
        // Particle dropped, content lemma kept.
        assert_eq!(
            analyze("昨日、公園を全力で走った。"),
            vec!["昨日", "公園", "全力", "走る"]
        );
        // Proper-noun-style tokens with an unassigned base form keep their surface.
        let units = analyze("富士山");
        assert_eq!(units, vec!["富士山"]);
    }

    #[test]
    fn vibrato_analysis_is_deterministic_and_idempotent() {
        let Some(()) = with_dictionary_if_present() else {
            eprintln!("SKIPPED (ipadic dictionary not fetched; see pocket-ic-tests resources)");
            return;
        };
        let fixtures = [
            "",
            "走った",
            "昨日、公園を全力で走った。",
            "Hello, World!",
            "㍿",
        ];
        for fixture in fixtures {
            let first = analyze(fixture);
            let second = analyze(fixture);
            assert_eq!(
                first, second,
                "analysis of {fixture:?} must be deterministic"
            );
            let replay = analyze(&first.join(" "));
            assert_eq!(
                replay, first,
                "re-analysis of {fixture:?} must be idempotent"
            );
        }
    }
}
