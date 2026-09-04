//! Shared per-candidate loader harness (used by both `tests/fixtures.rs` and
//! `tests/measure.rs`). Only candidates whose feature is enabled appear in
//! [`candidates`]; each measurement run enables exactly one candidate feature.

type Candidate = (&'static str, Box<dyn Fn(&str) -> Vec<String> + Send + Sync>);

#[allow(clippy::vec_init_then_push)] // cfg-gated pushes cannot use vec![..]
pub fn candidates() -> Vec<Candidate> {
    let mut v: Vec<Candidate> = Vec::new();
    #[cfg(feature = "rule")]
    v.push(("rule", Box::new(crate::rule::analyze)));
    #[cfg(feature = "vibrato")]
    v.push((
        "vibrato",
        Box::new(|t: &str| with_vibrato(|a| a.analyze(t))),
    ));
    #[cfg(feature = "sudachi")]
    v.push((
        "sudachi",
        Box::new(|t: &str| with_sudachi(|a| a.analyze(t))),
    ));
    #[cfg(feature = "lindera")]
    v.push((
        "lindera",
        Box::new(|t: &str| with_lindera(|a| a.analyze(t))),
    ));
    v
}

#[allow(dead_code)] // wasm builds never touch the filesystem loaders
const RESOURCES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/resources");

#[cfg(feature = "vibrato")]
fn with_vibrato<T>(f: impl FnOnce(&crate::vibrato_candidate::Analyzer) -> T) -> T {
    use std::sync::OnceLock;
    static ANALYZER: OnceLock<crate::vibrato_candidate::Analyzer> = OnceLock::new();
    let analyzer = ANALYZER.get_or_init(|| {
        crate::vibrato_candidate::Analyzer::from_zstd_file(
            std::path::Path::new(RESOURCES_DIR)
                .join("vibrato/system.dic.zst")
                .as_path(),
        )
        .expect("vibrato dictionary load")
    });
    f(analyzer)
}

#[cfg(feature = "sudachi")]
fn with_sudachi<T>(f: impl FnOnce(&crate::sudachi_candidate::Analyzer) -> T) -> T {
    use std::sync::OnceLock;
    static ANALYZER: OnceLock<crate::sudachi_candidate::Analyzer> = OnceLock::new();
    let analyzer = ANALYZER.get_or_init(|| {
        let raw =
            std::fs::read(std::path::Path::new(RESOURCES_DIR).join("sudachi/system_small.dic"))
                .expect("SudachiDict small present under resources/ (fetch_resources.sh)");
        crate::sudachi_candidate::Analyzer::from_bytes(&raw).expect("sudachi dictionary load")
    });
    f(analyzer)
}

#[cfg(feature = "lindera")]
fn with_lindera<T>(f: impl FnOnce(&crate::lindera_candidate::Analyzer) -> T) -> T {
    use std::sync::OnceLock;
    static ANALYZER: OnceLock<crate::lindera_candidate::Analyzer> = OnceLock::new();
    let analyzer = ANALYZER.get_or_init(|| {
        // Native: prebuilt dictionary dir (resources/lindera/lindera-ipadic). The wasm
        // measurement uses the `embedded://ipadic` path (see build_wasm.sh).
        crate::lindera_candidate::Analyzer::from_path(
            std::path::Path::new(RESOURCES_DIR)
                .join("lindera/lindera-ipadic")
                .as_path(),
        )
        .expect("lindera dictionary load")
    });
    f(analyzer)
}
