//! Plan 0333 — vendored MeCrab dict layer (github.com/cool-japan/mecrab @
//! 85444b56dd85e5f239396fbc574854d2b6d4137c, v0.1.0, MIT OR Apache-2.0), refactored
//! `Arc<Mmap>` → `Arc<dyn ByteImage>`.
//!
//! Vendored runtime-analysis path ONLY: dict/, lattice/, viterbi/analysis, normalize.rs.
//! EXCLUDED upstream modules (recorded as deletions): semantic/, word2vec (vectors.rs),
//! phonetic/, stream.rs, python.rs, wasm.rs, kizame, benches, fuzz, dict/user_dict.rs,
//! lattice/visualize.rs, viterbi/nbest.rs, viterbi/simd.rs, bench.rs, debug.rs.
//! Spike-authored here: byteimage.rs (ByteImage trait + HeapImage +
//! SimulatedStableImage) and the thin Analyzer glue below (adapted from upstream
//! lib.rs lines ~570-600, minus n-best/semantic/wasm surfaces).

pub mod byteimage;
pub mod dict;
pub mod error;
pub mod lattice;
pub mod normalize;
pub mod viterbi;

pub use byteimage::{AccessStats, ByteImage, HeapImage, SimulatedStableImage};
pub use dict::Dictionary;
pub use error::{Error, Result};
pub use lattice::Lattice;
pub use viterbi::{PathNode, ViterbiSolver};

use std::path::Path;
use std::sync::Arc;

/// Vendored MeCrab analyzer over a `ByteImage`-parameterized dictionary.
///
/// This is the spike parity/measure vehicle: `analyze` applies the LANDED unit-emission
/// rules (feature column 6 = 基本形 for content words; drop 助詞/助動詞/記号/接頭辞/
/// 接尾辞/フィラー) over the MeCrab feature accessors, and `analyze_tokens` exposes the
/// full segmentation for discrepancy classification.
pub struct MecrabAnalyzer {
    dictionary: Arc<Dictionary>,
}

impl MecrabAnalyzer {
    /// Native loader: four MeCab-format images from a directory into `HeapImage`.
    pub fn from_dir(path: &Path) -> Result<Self> {
        let dictionary = Arc::new(Dictionary::load(path)?);
        Ok(Self { dictionary })
    }

    /// Byte-image loader (the canister landing shape): the four images arrive behind
    /// arbitrary `ByteImage` impls (HeapImage / SimulatedStableImage).
    pub fn from_images(
        sys: Arc<dyn ByteImage>,
        unk: Arc<dyn ByteImage>,
        matrix: Arc<dyn ByteImage>,
        char_bin: Arc<dyn ByteImage>,
    ) -> Result<Self> {
        let dictionary = Arc::new(Dictionary::from_images(sys, unk, matrix, char_bin)?);
        Ok(Self { dictionary })
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    /// Full segmentation: (surface, feature) per morpheme, per line.
    pub fn analyze_tokens(&self, text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let lattice = match Lattice::build(line, &self.dictionary) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let solver = ViterbiSolver::new(&self.dictionary);
            let path: Vec<PathNode> = match solver.solve(&lattice) {
                Ok(p) => p,
                Err(_) => continue,
            };
            for node in path {
                out.push((node.surface, node.feature));
            }
        }
        out
    }

    /// Unit emission with the LANDED rules (same as `vibrato_candidate::Analyzer::analyze`).
    pub fn analyze(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (surface, feature) in self.analyze_tokens(text) {
            if surface.trim().is_empty() {
                continue;
            }
            let columns: Vec<&str> = feature.split(',').collect();
            let pos = columns.first().copied().unwrap_or("*");
            let base = columns.get(6).copied().unwrap_or("*");
            match pos {
                "助詞" | "助動詞" | "記号" | "接頭辞" | "接尾辞" | "フィラー" => continue,
                _ => {
                    if base == "*" || base.is_empty() {
                        out.push(surface);
                    } else {
                        out.push(base.to_string());
                    }
                }
            }
        }
        out
    }
}