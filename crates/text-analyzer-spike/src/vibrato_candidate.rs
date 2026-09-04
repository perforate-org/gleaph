//! Candidate B — vibrato 0.5.2 + ipadic-mecab compiled dictionary loaded from bytes.
//!
//! The distributed `system.dic.zst` is decompressed OUTSIDE the vibrato API (upstream
//! contract; `ruzstd` is used — a pure-Rust zstd decoder, so the path works on wasm too)
//! and the raw bytes are handed to `Dictionary::read`, the same bytes-based entry the
//! stable-resident landing shape (plan 0331) would use.
//!
//! Unit emission: the ipadic base form (feature column 6, 基本形) is emitted for
//! content words; 助詞/助動詞/記号/接頭辞/接尾辞/フィラー tokens are dropped.

use std::io::Read;

use vibrato::{Dictionary, Tokenizer};

use crate::prepass;

pub struct Analyzer {
    tokenizer: Tokenizer,
}

impl Analyzer {
    /// Load from RAW (decompressed) system dictionary bytes.
    pub fn from_bytes(raw_dict: &[u8]) -> Result<Self, vibrato::errors::VibratoError> {
        // `Tokenizer` owns the dictionary; nothing else references it, so no leak is
        // needed and the bytes-based load path stays exactly the 0331 landing shape.
        let dict = Dictionary::read(raw_dict)?;
        let tokenizer = Tokenizer::new(dict);
        Ok(Self { tokenizer })
    }

    /// Load from `resources/vibrato/ipadic-mecab-2_7_0/system.dic.zst`, decompressing
    /// with the pure-Rust `ruzstd` decoder (wasm-compatible; no C toolchain).
    pub fn from_zstd_file(path: &std::path::Path) -> anyhow::Result<Self> {
        let compressed = std::fs::read(path)?;
        let mut decoder = ruzstd::StreamingDecoder::new(&compressed[..])?;
        let mut raw = Vec::with_capacity(compressed.len() * 3);
        decoder.read_to_end(&mut raw)?;
        Ok(Self::from_bytes(&raw)?)
    }

    pub fn analyze(&self, text: &str) -> Vec<String> {
        let normalized = prepass(text);
        let mut out = Vec::new();
        for line in normalized.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut worker = self.tokenizer.new_worker();
            worker.reset_sentence(line);
            worker.tokenize();
            for token in worker.token_iter() {
                if token.surface().trim().is_empty() {
                    continue; // whitespace token (from the joined-unit re-analysis)
                }
                let feature = token.feature();
                let columns: Vec<&str> = feature.split(',').collect();
                let pos = columns.first().copied().unwrap_or("*");
                let base = columns.get(6).copied().unwrap_or("*");
                match pos {
                    "助詞" | "助動詞" | "記号" | "接頭辞" | "接尾辞" | "フィラー" =>
                    {
                        continue;
                    }
                    _ => {
                        if base == "*" || base.is_empty() {
                            out.push(token.surface().to_string());
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
