//! Dictionary-analyzer engine (plan 0330 decision B, plan 0333 proof, plan 0334
//! landing; plan 0341 widens to id 3): MeCab-format Viterbi lemma units over the
//! `morph-dict` engine with the `ic-morph-dict` stable-memory adapter. Id 2 runs the
//! ipadic profile; id 3 (`ANALYZER_KOREAN`) runs the mecab-ko-dic profile — the profile
//! is bound once at load (`profile_for`), the resident `Analyzer` carries it, and
//! dispatch needs no per-call branch.
//!
//! Pipeline: whole-text pre-pass (the shared [`morph_dict::normalize`] module: NFKC +
//! Unicode lowercase + variation-selector strip — the v1 analyzer's normalization order,
//! applied to the whole text so word adjacency survives for the dictionary; the strip is
//! safe pre-tokenization because no ipadic surface contains a variation selector, pinned
//! by the dictionary field-scan test below), then per-line Viterbi tokenization, emitting
//! the Japanese DictionaryProfile lemma (ipadic 基本形, feature column 6) for content
//! words. 助詞/助動詞/記号/接頭辞/接尾辞/フィラー tokens are dropped; a token whose lemma is the
//! unassigned marker `*` emits its surface. The kana counter-variant fold (ヶ/ヵ → ケ,
//! plan 0339) applies to the EMITTED units only — never to the pre-tokenization input,
//! because ipadic surfaces contain ヶ (茅ヶ崎, 関ヶ原) and folding pre-tokenization would
//! rewrite the input away from the dictionary and damage Viterbi matching.
//! Output is a deterministic function of the input and strict idempotence holds:
//! `analyze(units.join(" ")) == analyze(input)` for every fixture (whitespace surface
//! tokens are skipped, so re-analyzing the space-joined units reproduces the units; the
//! folded units re-tokenize to themselves because ipadic carries the ケ-counterpart
//! surfaces).
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

use morph_dict::DictionaryProfile;
#[cfg(test)]
use morph_dict::byteimage::{ByteImage, HeapImage};

thread_local! {
    /// The pinned analyzer over the finalized MeCab-format container. `None` until the
    /// stable dictionary is finalized (and loaded); analyzer 2 operations fail closed
    /// before that point.
    static ANALYZER: RefCell<Option<morph_dict::Analyzer>> = const { RefCell::new(None) };
}

/// Unit-emission profile for a dictionary-carrying analyzer id (plan 0341): ids 0
/// and 2 share the ipadic container/profile; id 3 (`ANALYZER_KOREAN`) carries the
/// mecab-ko-dic container under the Korean profile. Unknown ids fail closed — callers
/// validate the id at the open/admission boundaries before reaching here.
pub fn profile_for(analyzer_id: u32) -> DictionaryProfile {
    match analyzer_id {
        crate::analyzer::ANALYZER_KOREAN => DictionaryProfile::korean_mecab_ko_dic(),
        crate::analyzer::ANALYZER_MULTILINGUAL | crate::analyzer::ANALYZER_MECAB => {
            DictionaryProfile::japanese_ipadic()
        }
        other => panic!(
            "mecab profile requested for unregistered analyzer id {other} — broken invariant"
        ),
    }
}

/// Builds the pinned analyzer from a container IMAGE (the durable region 16, addressed
/// through any `ByteImage` — production: the `ic-morph-dict` `StableImage`). Structural
/// validation + resident-set materialization happen here; the container image must stay
/// valid for the process lifetime (the lazy feature reads go through it).
///
/// Fails closed on any validation miss — the caller must not record Finalized state
/// (and must not open) for bytes that fail validation.
pub fn load_dictionary_from_image<M>(
    analyzer_id: u32,
    region_image: ic_morph_dict::CanisterStableImage<M>,
) -> Result<(), String>
where
    M: ic_stable_structures::Memory + 'static,
{
    let analyzer = morph_dict::Analyzer::open(Arc::new(region_image), profile_for(analyzer_id))
        .map_err(|error| format!("mecab dictionary load failed (corrupt artifact): {error:?}"))?;
    ANALYZER.with(|slot| {
        *slot.borrow_mut() = Some(analyzer);
    });
    Ok(())
}

/// Builds the pinned analyzer from an in-memory container (test path, ipadic profile).
#[cfg(test)]
pub fn load_dictionary_bytes(container: &[u8]) -> Result<(), String> {
    load_dictionary_bytes_for(crate::analyzer::ANALYZER_MECAB, container)
}

/// Builds the pinned analyzer from an in-memory container under an explicit analyzer
/// id (tests — the plan-0341 Korean path binds the ko-dic profile at load).
#[cfg(test)]
pub fn load_dictionary_bytes_for(analyzer_id: u32, container: &[u8]) -> Result<(), String> {
    load_dictionary_from_heap(
        analyzer_id,
        Arc::new(HeapImage::from_vec(container.to_vec())),
    )
}

/// Variant taking any heap-backed image under an explicit analyzer id (tests).
#[cfg(test)]
pub fn load_dictionary_from_heap(
    analyzer_id: u32,
    image: Arc<dyn ByteImage>,
) -> Result<(), String> {
    let analyzer = morph_dict::Analyzer::open(image, profile_for(analyzer_id))
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
    // Whole-text shared pre-pass (plan 0339 SSOT): NFKC + lowercase + variation-selector
    // strip. The strip is safe pre-tokenization — no ipadic surface (entry text) contains
    // a variation selector (pinned by `ipadic_dictionary_fields_contain_no_variation_selectors`
    // below). The KANA fold is deliberately NOT applied here: ipadic surfaces contain ヶ
    // (茅ヶ崎, 関ヶ原), so pre-tokenization folding would rewrite the input away from the
    // dictionary; the fold applies to the EMITTED units below (plan 0339 key design
    // decision — dictionary fidelity).
    let normalized = morph_dict::normalize::prepass(text);
    let mut units = analyzer.analyze(&normalized);
    morph_dict::normalize::fold_units(&mut units);
    units
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

    /// Plan 0341 todo 2 (text side): the Korean path binds the ko-dic profile at load
    /// and recalls through the same `analyze` entry point. SKIPs loudly when the
    /// ko-dic images are absent (same fetch-once pattern as the ipadic cache).
    #[test]
    fn korean_profile_recalls_headline_through_canister_entry_point() {
        let dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pocket-ic-tests/resources/mecab-ko-dic"
        ));
        let mut images: Vec<(String, Vec<u8>)> = Vec::new();
        for n in ["sys.dic", "unk.dic", "matrix.bin", "char.bin"] {
            let Ok(bytes) = std::fs::read(dir.join(n)) else {
                eprintln!(
                    "SKIPPED (ko-dic images not built; see pocket-ic-tests/resources/mecab-ko-dic)"
                );
                return;
            };
            images.push((n.to_string(), bytes));
        }
        let container = morph_dict::container::build(images);
        load_dictionary_bytes_for(crate::analyzer::ANALYZER_KOREAN, &container)
            .expect("ko-dic container loads under the Korean profile");
        // Headline: 학교 from 학교에서 (조사 에서 dropped by the PREFIX drop set);
        // the dispatch arm routes id 3 through this same entry point.
        assert_eq!(
            crate::analyzer::analyze_pinned(crate::analyzer::ANALYZER_KOREAN, "학교에서"),
            vec!["학교"]
        );
    }

    // -- Plan 0339: IVS-strip safety + emitted-unit fold fidelity ----------------------------

    /// Verification method (plan 0339, recorded): the ipadic images are scanned at test
    /// time for variation selectors. A raw byte scan is NOT the predicate — sys.dic's
    /// binary table regions contain two coincidental 4-byte collisions that decode as
    /// nothing — so the check scans every NUL-delimited chunk and flags a selector only
    /// inside a chunk that is VALID UTF-8: the only way a selector could take part in
    /// tokenization is by sitting in a text field (feature string) or being matched as
    /// input text, and the pre-pass strips selectors from input BEFORE the lattice.
    /// The pin: zero valid-UTF-8 fields carry a selector across all four images, so
    /// stripping input can never rewrite a dictionary surface away (the strip is safe
    /// pre-tokenization).
    #[test]
    fn ipadic_dictionary_fields_contain_no_variation_selectors() {
        let dir = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../pocket-ic-tests/resources/mecrab"
        ));
        for name in ["sys.dic", "unk.dic", "matrix.bin", "char.bin"] {
            let Ok(bytes) = std::fs::read(dir.join(name)) else {
                eprintln!("SKIPPED (mecab dictionary not fetched; see pocket-ic-tests resources)");
                return;
            };
            for chunk in bytes.split(|b| *b == 0) {
                let Ok(field) = std::str::from_utf8(chunk) else {
                    continue; // binary region: selectors there cannot reach analysis
                };
                assert!(
                    !field
                        .chars()
                        .any(morph_dict::normalize::is_variation_selector_for_scan),
                    "{name} carries a variation selector in a valid-UTF-8 field: {field:?}"
                );
            }
        }
    }

    /// THE design-decision pin (plan 0339 key decision): the fold unifies only the
    /// OUTPUT space. Tokenization must see the original codepoint: 三ヶ月 has no
    /// single ipadic entry and tokenizes as the unknown 数詞 三 + the ipadic
    /// 助数詞 suffix ヶ月 — TWO units, whose emitted suffix folds to ケ月. The wrong
    /// implementation (folding pre-tokenization input) rewrites that lattice unit
    /// ヶ月 → ケ月 before the Viterbi match — a dictionary-identity rewrite, observed
    /// here through `analyze_tokens`, the raw pre-fold (surface, feature) stream.
    /// The same-word ケ spelling (三ケ月) emits the identical folded unit, so the
    /// output space unifies across the ヶ/ケ spelling.
    #[test]
    fn fold_applies_to_emitted_units_never_to_pre_tokenization_input() {
        let Some(()) = with_container_if_present() else {
            eprintln!("SKIPPED (mecab dictionary not fetched; see pocket-ic-tests resources)");
            return;
        };
        // The raw pre-fold tokenization matches the ipadic suffix entry ヶ月 — the
        // unit identity pre-tokenization folding would have rewritten (wrong-impl
        // detector: a pre-tokenization fold changes this stream to ケ月).
        let raw = ANALYZER
            .with(|slot| {
                slot.borrow()
                    .as_ref()
                    .map(|analyzer| analyzer.analyze_tokens("三ヶ月"))
            })
            .expect("dictionary resident");
        assert_eq!(
            raw,
            vec![
                ("三".to_string(), "名詞,数,*,*,*,*,三,サン,サン".to_string()),
                (
                    "ヶ月".to_string(),
                    "名詞,接尾,助数詞,*,*,*,ヶ月,カゲツ,カゲツ".to_string()
                ),
            ],
            "pre-fold tokenization must see the original ヶ codepoint: {raw:?}"
        );
        // Output space: the same input's EMITTED units carry the folded suffix, and
        // the same-word ケ spelling emits the identical folded unit.
        assert_eq!(analyze("三ヶ月"), vec!["三", "ケ月"]);
        assert_eq!(analyze("三ケ月"), vec!["三", "ケ月"]);
        assert_eq!(
            analyze("三ヶ月"),
            analyze("三ケ月"),
            "emitted units unify across the ヶ/ケ spelling"
        );
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
            // Plan 0339: folded units re-tokenize to themselves (ケ月 is an ipadic
            // surface), so strict idempotence survives the fold; 葛󠄀 pins the strip.
            "3ヶ月",
            "茅ヶ崎",
            "葛\u{E0100}飾区",
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

    /// Pins the id-2 emissions the E2E folding leg (plan 0339 todo 3) asserts on, so
    /// the PocketIC leg's expectations are grounded in the engine's actual behavior.
    #[test]
    fn mecab_emission_for_folding_e2e_docs() {
        let Some(()) = with_container_if_present() else {
            eprintln!("SKIPPED (mecab dictionary not fetched; see pocket-ic-tests resources)");
            return;
        };
        // 3ヶ月 doc: the 助数詞 suffix ヶ月 folds to ケ月.
        assert_eq!(analyze("3ヶ月"), vec!["3", "ケ月"]);
        // 3か月 doc: the 助数詞 suffix か月 matches after the numeral and is NOT
        // folded (か ≠ ヶ/ヵ) — the v1 boundary the E2E negative asserts.
        assert_eq!(analyze("3か月"), vec!["3", "か月"]);
        // The IVS doc strips to its bare base form before the lattice.
        assert_eq!(analyze("葛\u{E0100}区"), analyze("葛区"));
        // The standalone counter units the queries emit.
        assert_eq!(analyze("ケ月"), vec!["ケ月"]);
        assert_eq!(analyze("か月"), vec!["か月"]);
    }

    /// Koine parity (plan 0339 todo 2): the multilingual composite (id 0) routes its
    /// {kanji∪kana} runs through the mecab layer, so the kana counter-variant fold
    /// applies there too. The digit 3 is a separate UAX #29 segment, so the composite
    /// hands only the run ヶ月 to mecab; the ipadic 助数詞 suffix cannot match at BOS
    /// and splits into ケ+月 — the fold still unifies 3ヶ月 and ３ケ月 to the SAME
    /// emitted units, and the hiragana counter 3か月 stays distinct (v1 boundary).
    /// (The plan's [3][ケ月] bigram shape is the id-1 analyzer.rs contract, asserted
    /// in `analyzer::tests::kana_counter_variants_fold_before_bigram_formation`.)
    #[test]
    fn koine_composite_folds_kana_counter_variants() {
        let Some(()) = with_container_if_present() else {
            eprintln!("SKIPPED (mecab dictionary not fetched; see pocket-ic-tests resources)");
            return;
        };
        assert_eq!(
            crate::analyzer_multilingual::analyze("3ヶ月"),
            vec!["3", "ケ", "月"],
            "koine folds the ヶ counter through its mecab layer"
        );
        assert_eq!(
            crate::analyzer_multilingual::analyze("３ケ月"),
            vec!["3", "ケ", "月"],
            "fullwidth NFKC + fold chain composes on the composite"
        );
        assert_eq!(
            crate::analyzer_multilingual::analyze("3ヶ月"),
            crate::analyzer_multilingual::analyze("３ケ月"),
            "3ヶ月 and ３ケ月 unify on the composite"
        );
        // The v1 boundary survives the composite: か is the particle か (dropped by the
        // mecab layer), never folded to ケ — 3か月 emits no ケ unit at all.
        assert_eq!(
            crate::analyzer_multilingual::analyze("3か月"),
            vec!["3", "月"],
            "hiragana counter stays distinct (v1 boundary)"
        );
        // The composite's own pre-pass now routes through the shared module, so it
        // strips variation selectors too (parity with ids 1/2): the IVS-bearing
        // pure-Han run analyzes byte-identically to its bare base form.
        assert_eq!(
            crate::analyzer_multilingual::analyze("葛\u{E0100}区"),
            crate::analyzer_multilingual::analyze("葛区"),
            "koine strips IVS through the shared pre-pass (parity fix)"
        );
    }
}
