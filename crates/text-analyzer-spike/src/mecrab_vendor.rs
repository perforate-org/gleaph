//! Landing shim (plan 0334): the vendored MeCrab dict layer moved out of the spike into
//! the `morph-dict` crate (plus the `ic-morph-dict` stable-memory adapter). This module
//! keeps the plan-0333 test/harness call sites (`text_analyzer_spike::mecrab_vendor::…`)
//! working over that crate.

use std::path::Path;
use std::sync::Arc;

pub use morph_dict::*;

/// The 0333-era analyzer surface over [`morph_dict::Analyzer`]: opened from a directory
/// holding the four MeCab images, analyzing to plain units.
pub struct MecrabAnalyzer(Analyzer);

impl MecrabAnalyzer {
    /// Opens `sys.dic`, `unk.dic`, `matrix.bin` and `char.bin` from `dir`.
    pub fn from_dir(dir: &Path, profile: DictionaryProfile) -> anyhow::Result<Self> {
        let images = ImageKind::ALL
            .iter()
            .map(|kind| Ok((kind.file_name(), std::fs::read(dir.join(kind.file_name()))?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let image: Arc<dyn ByteImage> = Arc::new(HeapImage::from_vec(container::build(&images)?));
        Ok(Self(Analyzer::open(&image, profile)?))
    }

    /// The units of `text`; a line the analyzer refuses (over the line bound) yields none.
    pub fn analyze(&self, text: &str) -> Vec<String> {
        self.0.analyze(text).unwrap_or_default()
    }
}
