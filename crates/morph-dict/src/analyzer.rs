//! The morph-dict analyzer: container-backed dictionary + profile-driven unit emission.
//!
//! `open` builds the analyzer from a MORPHDICT1 container image with the measurement-
//! driven residency split: matrix.bin + char.bin + unk.dic and sys.dic's trie/word-params
//! region are copied to the heap (the random 2–16 B hot path, syscall-hostile); sys.dic's
//! feature-string region stays LAZY over the supplied container image (output-path reads
//! only, after the Viterbi path is chosen). Callers on the IC pass the `ic-morph-dict`
//! stable-memory `ByteImage`; native callers pass a `HeapImage`.

use std::path::Path;
use std::sync::Arc;

use crate::byteimage::{ByteImage, HeapImage, OffsetImage};
use crate::container::{self, ContainerEntry};
use crate::dict::Dictionary;
use crate::error::Result;
use crate::lattice::Lattice;
use crate::profile::DictionaryProfile;
use crate::viterbi::{PathNode, ViterbiSolver};

/// Container entry names for the four MeCab-format images.
pub const SYS_DIC: &str = "sys.dic";
pub const UNK_DIC: &str = "unk.dic";
pub const MATRIX_BIN: &str = "matrix.bin";
pub const CHAR_BIN: &str = "char.bin";

/// A pinned analyzer: dictionary + unit-emission profile. Cloneable.
#[derive(Clone)]
pub struct Analyzer {
    dictionary: Arc<Dictionary>,
    profile: DictionaryProfile,
}

impl Analyzer {
    /// Structural validation of a container image (magic + entry table + bounds).
    /// Cheap: header/table reads only. Called at finalize AND at open/rebind before
    /// any materialization.
    pub fn validate_container(container: &dyn ByteImage) -> Result<Vec<ContainerEntry>> {
        container::validate(container)
    }

    /// Builds the analyzer over a MORPHDICT1 container image with the default residency
    /// policy: matrix.bin + char.bin + unk.dic fully resident, sys.dic split at its
    /// feature-region boundary (resident prefix = header + trie + word params; feature
    /// region lazy over `container`). The container image must stay alive (it backs the
    /// lazy feature reads).
    pub fn open(container: Arc<dyn ByteImage>, profile: DictionaryProfile) -> Result<Self> {
        let entries = container::validate(container.as_ref())?;
        let sys = container::entry(&entries, SYS_DIC)?;
        let unk = container::entry(&entries, UNK_DIC)?;
        let matrix = container::entry(&entries, MATRIX_BIN)?;
        let char_bin = container::entry(&entries, CHAR_BIN)?;

        // Feature-region boundary from the sys.dic header alone (no materialization).
        let sys_view = OffsetImage::new(Arc::clone(&container), sys.offset, sys.len);
        let feature_offset =
            crate::dict::sys_dic::SysDic::header_feature_offset(&sys_view)?;

        // Residency policy: sys.dic's header + trie + word-params region is copied to
        // the heap (random 2–16 B hot path, syscall-hostile, contiguous fast path); the
        // feature-string region stays LAZY over the container image (output-path reads
        // only). matrix.bin and char.bin are fully materialized by their loaders.
        let mut prefix = vec![0u8; feature_offset as usize];
        sys_view.read_exact_at(0, &mut prefix);
        let feature_image = OffsetImage::new(
            Arc::clone(&container),
            sys.offset + feature_offset,
            sys.len - feature_offset,
        );
        let sys_image = crate::dict::sys_dic::SysDic::from_parts(
            Arc::new(HeapImage::from_vec(prefix)),
            Arc::new(feature_image),
            (sys.len - feature_offset) as usize,
        )?;

        let unk_image = read_heap(container.as_ref(), unk)?;
        let matrix_image = read_heap(container.as_ref(), matrix)?;
        let char_image = read_heap(container.as_ref(), char_bin)?;

        let dictionary = Arc::new(Dictionary::from_parts(
            sys_image,
            unk_image,
            matrix_image,
            char_image,
        )?);
        Ok(Self {
            dictionary,
            profile,
        })
    }

    /// Directory loader (native tooling/tests): four MeCab-format image files, fully
    /// resident (whole sys.dic copy — the native analogue of full residency).
    pub fn from_dir(path: &Path, profile: DictionaryProfile) -> Result<Self> {
        let dictionary = Arc::new(Dictionary::load(path)?);
        Ok(Self {
            dictionary,
            profile,
        })
    }

    pub fn dictionary(&self) -> &Dictionary {
        &self.dictionary
    }

    pub fn profile(&self) -> &DictionaryProfile {
        &self.profile
    }

    /// Full segmentation: (surface, feature) per morpheme, per line. A line exceeding
    /// the profile's `max_line_bytes` fails closed (panic) — see the lattice guard.
    pub fn analyze_tokens(&self, text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let lattice = match Lattice::build(line, &self.dictionary, &self.profile) {
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

    /// Profile-driven unit emission (the `analyze_pinned(2, text)` semantics once the
    /// caller applies its pre-pass): 品詞1 drop set filtered, lemma column emitted,
    /// `*`/empty lemma falls back to the surface. Whitespace-only surfaces (from the
    /// space-joined-unit re-analysis) are skipped — this makes the emission idempotent.
    /// Fused over the Viterbi path (no intermediate token vector).
    pub fn analyze(&self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let lattice = match Lattice::build(line, &self.dictionary, &self.profile) {
                Ok(l) => l,
                Err(_) => continue,
            };
            let solver = ViterbiSolver::new(&self.dictionary);
            let path: Vec<PathNode> = match solver.solve(&lattice) {
                Ok(p) => p,
                Err(_) => continue,
            };
            for node in path {
                if node.surface.trim().is_empty() {
                    continue;
                }
                let mut columns = node.feature.split(',');
                let pos = columns.next().unwrap_or("*");
                if self.profile.drops(pos) {
                    continue;
                }
                match self
                    .profile
                    .lemma_column
                    .and_then(|c| columns.nth(c - 1))
                {
                    Some(base) if base != "*" && !base.is_empty() => out.push(base.to_string()),
                    _ => out.push(node.surface.to_string()),
                }
            }
        }
        out
    }
}

fn read_heap(container: &dyn ByteImage, entry: &ContainerEntry) -> Result<Arc<dyn ByteImage>> {
    let mut bytes = vec![0u8; entry.len as usize];
    container.read_exact_at(entry.offset, &mut bytes);
    Ok(Arc::new(HeapImage::from_vec(bytes)))
}