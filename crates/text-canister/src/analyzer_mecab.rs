//! ANALYZER_ID=2 pipeline: MeCab-format ipadic Viterbi lemma units (plan 0330 decision B,
//! plan 0333 proof, plan 0334 landing) over the `morph-dict` engine with the
//! `ic-morph-dict` stable-memory adapter.
//!
//! Pipeline: whole-text NFKC + Unicode lowercase pre-pass (the v1 analyzer's normalization
//! order, applied to the whole text so word adjacency survives for the dictionary), then
//! per-line Viterbi tokenization, emitting the Japanese DictionaryProfile lemma (ipadic
//! 基本形, feature column 6) for content words. 助詞/助動詞/記号/接頭辞/接尾辞/フィラー
//! tokens are dropped; a token whose lemma is the unassigned marker `*` emits its surface.
//! Output is a deterministic function of the input and strict idempotence holds:
//! `analyze(units.join(" ")) == analyze(input)` for every fixture (whitespace surface
//! tokens are skipped, so re-analyzing the space-joined units reproduces the units).
//!
//! Dictionary lifecycle (plan 0334): region 16 carries the MPD container
//! (magic + entry table {name, offset, len, sha256} + the four MeCab-format images:
//! sys.dic + unk.dic + matrix.bin + char.bin, 52,930,923 bytes for ipadic 2.7.0 utf8).
//! Open/finalize rebind = structural validation + resident-set memcpy over batched
//! stable reads — NO decode, NO full-container copy: the feature-string region stays
//! lazy over the durable region via the `ic-morph-dict` `StableImage` (measured hot set:
//! ~472 KiB of feature pages per MB of text). The resident set is ~21 MB
//! (matrix.bin 3.46 + char.bin 0.26 + unk.dic ~0 + sys.dic trie/word-params ~17.7 MB).

use std::cell::RefCell;
use std::sync::Arc;

use unicode_normalization::UnicodeNormalization;

use morph_dict::DictionaryProfile;
#[cfg(test)]
use morph_dict::byteimage::{ByteImage, HeapImage};

thread_local! {
    /// The pinned analyzer over the finalized MeCab-format container. `None` until the
    /// stable dictionary is finalized (and loaded); analyzer 2 operations fail closed
    /// before that point.
    static ANALYZER: RefCell<Option<morph_dict::Analyzer>> = const { RefCell::new(None) };
}

fn profile() -> DictionaryProfile {
    DictionaryProfile::japanese_ipadic()
}

/// Builds the pinned analyzer from a container IMAGE (the durable region 16, addressed
/// through any `ByteImage` — production: the `ic-morph-dict` `StableImage`). Structural
/// validation + resident-set materialization happen here; the container image must stay
/// valid for the process lifetime (the lazy feature reads go through it).
///
/// Fails closed on any validation miss — the caller must not record Finalized state
/// (and must not open) for bytes that fail validation.
pub fn load_dictionary_from_image<M>(
    region_image: ic_morph_dict::CanisterStableImage<M>,
) -> Result<(), String>
where
    M: ic_stable_structures::Memory + 'static,
{
    let analyzer = morph_dict::Analyzer::open(Arc::new(region_image), profile())
        .map_err(|error| format!("mecab dictionary load failed (corrupt artifact): {error:?}"))?;
    ANALYZER.with(|slot| {
        *slot.borrow_mut() = Some(analyzer);
    });
    Ok(())
}

/// Builds the pinned analyzer from an in-memory container (test path).
#[cfg(test)]
pub fn load_dictionary_bytes(container: &[u8]) -> Result<(), String> {
    load_dictionary_from_heap(Arc::new(HeapImage::from_vec(container.to_vec())))
}

/// Variant taking any heap-backed image (tests).
#[cfg(test)]
pub fn load_dictionary_from_heap(image: Arc<dyn ByteImage>) -> Result<(), String> {
    let analyzer = morph_dict::Analyzer::open(image, profile())
        .map_err(|error| format!("mecab dictionary load failed (corrupt artifact): {error:?}"))?;
    ANALYZER.with(|slot| {
        *slot.borrow_mut() = Some(analyzer);
    });
    Ok(())
}

/// Whether the pinned analyzer is resident. Open-time validation guarantees it is
/// loaded exactly when meta pins analyzer 2 AND the dictionary is finalized, so this
/// is diagnostic-only.
#[cfg_attr(not(test), allow(dead_code))]
pub fn dictionary_loaded() -> bool {
    ANALYZER.with(|slot| slot.borrow().is_some())
}

/// Drops the resident analyzer (test helper; production reloads only at finalize/open).
#[cfg(test)]
#[allow(dead_code)]
pub fn reset() {
    ANALYZER.with(|slot| *slot.borrow_mut() = None);
}

/// Analyzes `text` into ipadic lemma units. Panics fail-closed when the dictionary is
/// not resident — callers gate every analyze-touching operation on the finalized
/// dictionary state (open validation + ingest/search preflight).
pub fn analyze(text: &str) -> Vec<String> {
    let analyzer_held = ANALYZER.with(|slot| slot.borrow().is_some());
    assert!(
        analyzer_held,
        "analyzer 2 (mecab) used without a finalized MeCab-format dictionary — \
         broken open/finalize invariant"
    );
    ANALYZER.with(|slot| {
        let slot = slot.borrow();
        let analyzer = slot.as_ref().expect("checked above");
        analyze_with(analyzer, text)
    })
}

/// Analyzes pre-normalized text with `analyzer` (unit-testable core).
fn analyze_with(analyzer: &morph_dict::Analyzer, text: &str) -> Vec<String> {
    // Whole-text NFKC + lowercase, matching the v1 pipeline's normalization order.
    let normalized = text.nfkc().collect::<String>().to_lowercase();
    analyzer.analyze(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built container path (gitignored; built from the four MeCab-format images under
    /// `pocket-ic-tests/resources/mecrab/` by the plan 0334 E2E fetch). Tests needing
    /// the dictionary SKIP (loudly) when the artifact is absent so the suite stays
    /// runnable without the 53 MB download; the E2E fetches it fail-closed.
    fn with_container_if_present() -> Option<()> {
        let dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pocket-ic-tests/resources/mecrab"
        ));
        let names = ["sys.dic", "unk.dic", "matrix.bin", "char.bin"];
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        for n in names {
            images.push((n.to_string(), std::fs::read(dir.join(n)).ok()?));
        }
        let container = morph_dict::container::build(images);
        load_dictionary_bytes(&container).ok()?;
        Some(())
    }

    #[test]
    fn load_rejects_corrupt_bytes_without_residency() {
        assert!(load_dictionary_bytes(b"not a dictionary").is_err());
        assert!(!dictionary_loaded());
    }

    #[test]
    fn mecab_unit_emission_fixtures() {
        let Some(()) = with_container_if_present() else {
            eprintln!("SKIPPED (mecab dictionary not fetched; see pocket-ic-tests resources)");
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
    fn mecab_analysis_is_deterministic_and_idempotent() {
        let Some(()) = with_container_if_present() else {
            eprintln!("SKIPPED (mecab dictionary not fetched; see pocket-ic-tests resources)");
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
