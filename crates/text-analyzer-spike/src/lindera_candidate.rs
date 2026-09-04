//! Candidate D — lindera 6.0 + ipadic.
//!
//! LANDING-SHAPE FINDING (recorded in plan 0330): lindera 6.0's system-dictionary API
//! is PATH-ONLY (`lindera_dictionary::dictionary::Dictionary::load_from_path` /
//! `load_dictionary_with_options("file://…")`); the only bytes-based entry is
//! `UserDictionary::load(&[u8])`, which is for user dictionaries, not the system
//! dictionary. Therefore D's only landing shape is the **wasm-embedded dictionary**
//! (`lindera-ipadic` `embed-ipadic` feature, `embedded://ipadic` URI) — measured as such.
//!
//! Unit emission mirrors candidate B: ipadic lemma (detail column 7, 基本形) for content
//! words; 助詞/助動詞/記号/接頭辞/接尾辞 dropped.

use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;

use crate::prepass;

pub struct Analyzer {
    segmenter: Segmenter,
}

impl Analyzer {
    /// Embedded dictionary (`embed-ipadic` feature; build.rs downloads/builds the
    /// dictionary source at compile time). This is the wasm-embeddable path.
    pub fn embedded() -> anyhow::Result<Self> {
        let dictionary = load_dictionary("embedded://ipadic")?;
        Ok(Self {
            segmenter: Segmenter::new(Mode::Normal, dictionary, None),
        })
    }

    /// Path-based prebuilt dictionary directory (native measurement convenience).
    pub fn from_path(path: &std::path::Path) -> anyhow::Result<Self> {
        let dictionary = lindera::dictionary::Dictionary::load_from_path(path)?;
        Ok(Self {
            segmenter: Segmenter::new(Mode::Normal, dictionary, None),
        })
    }

    pub fn analyze(&self, text: &str) -> Vec<String> {
        let normalized = prepass(text);
        let mut out = Vec::new();
        for line in normalized.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut tokens = self
                .segmenter
                .segment(line.into())
                .expect("lindera tokenization");
            for token in tokens.iter_mut() {
                if token.surface.trim().is_empty() {
                    continue; // whitespace token (from the joined-unit re-analysis)
                }
                let details = token.details();
                let pos = details[0];
                let base = if details.len() > 6 { details[6] } else { "*" };
                match pos {
                    "助詞" | "助動詞" | "記号" | "接頭辞" | "接尾辞" | "フィラー" =>
                    {
                        continue;
                    }
                    _ => {
                        if base == "*" || base.is_empty() {
                            out.push(token.surface.to_string());
                        } else {
                            out.push(base.to_string());
                        }
                    }
                }
            }
        }
        out
    }
}
