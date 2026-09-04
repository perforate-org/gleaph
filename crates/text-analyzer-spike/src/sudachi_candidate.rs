//! Candidate C — sudachi.rs 0.6.11 (via the `suiko-sudachi` crates.io redistribution)
//! + SudachiDict small, system dictionary loaded from BYTES.
//!
//! Bytes-based loading exists in sudachi.rs's own API surface
//! (`SudachiDicData::new(Storage::Owned(bytes))` + `from_cfg_storage_with_embedded_chardef`,
//! which uses the crate-embedded default `char.def`) — this is exactly the
//! stable-resident landing shape plan 0331 would use.
//!
//! Unit emission: the SudachiDict normalized form (表記ゆれ solver: 附属→付属) for
//! content words (動詞/形容詞/名詞/副詞/連体詞/接続詞/感動詞), the surface for その他;
//! 助詞/助動詞/記号/補助記号/接頭辞/接尾辞 are dropped.

use std::sync::Arc;

use sudachi::analysis::stateless_tokenizer::StatelessTokenizer;
use sudachi::analysis::{Mode, Tokenize};
use sudachi::config::Config;
use sudachi::dic::dictionary::JapaneseDictionary;
use sudachi::dic::storage::{Storage, SudachiDicData};

use crate::prepass;

pub struct Analyzer {
    tokenizer: StatelessTokenizer<Arc<JapaneseDictionary>>,
}

impl Analyzer {
    /// Load from RAW SudachiDict system-dictionary bytes (e.g. `system_small.dic`).
    pub fn from_bytes(raw_dict: &[u8]) -> anyhow::Result<Self> {
        // Minimal config: ONLY the statically-bundled SimpleOovPlugin (no external
        // char.def / unk.def / matrix files needed beyond the embedded char def).
        let cfg = Config::minimal_at(std::path::Path::new("."));
        let storage = SudachiDicData::new(Storage::Owned(raw_dict.to_vec()));
        let dic = JapaneseDictionary::from_cfg_storage_with_embedded_chardef(&cfg, storage)?;
        let tokenizer = StatelessTokenizer::new(Arc::new(dic));
        Ok(Self { tokenizer })
    }

    pub fn analyze(&self, text: &str) -> Vec<String> {
        let normalized = prepass(text);
        let mut out = Vec::new();
        // Sudachi bounds a single tokenize call (~0xBFFF bytes); split at sentence
        // punctuation and hard-cap chunk length deterministically.
        for chunk in chunks(&normalized) {
            if chunk.trim().is_empty() {
                continue;
            }
            let morphemes = self
                .tokenizer
                .tokenize(chunk, Mode::B, false)
                .expect("sudachi tokenization");
            for morpheme in morphemes.iter() {
                if morpheme.surface().trim().is_empty() {
                    continue; // whitespace morpheme (from the joined-unit re-analysis)
                }
                let pos = morpheme.part_of_speech();
                if pos.is_empty() {
                    continue;
                }
                match pos[0].as_str() {
                    "助詞" | "助動詞" | "補助記号" | "接頭辞" | "接尾辞" | "フィラー" =>
                    {
                        continue;
                    }
                    _ => {}
                }
                let normalized_form = morpheme.normalized_form();
                if normalized_form.is_empty() {
                    out.push(morpheme.surface().to_string());
                } else {
                    out.push(normalized_form.to_string());
                }
            }
        }
        out
    }
}

/// Split into chunks ≤ 40_000 bytes, preferring sentence-final 。/！/？ boundaries so the
/// split is deterministic and never splits inside a word.
fn chunks(text: &str) -> Vec<&str> {
    const LIMIT: usize = 40_000;
    if text.len() <= LIMIT {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let hard_end = (start + LIMIT).min(text.len());
        // Snap to a char boundary first.
        let mut end = hard_end;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            // Prefer the last sentence boundary inside the window.
            if let Some(pos) = text[start..end].rfind(['。', '！', '？', '\n']) {
                let boundary_char = text[start + pos..end].chars().next().unwrap_or(' ');
                end = start + pos + boundary_char.len_utf8();
            }
        }
        let _ = hard_end;
        if end <= start {
            end = (start + LIMIT).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
        }
        out.push(&text[start..end]);
        start = end;
    }
    out
}
