//! Deterministic benchmark corpus generation.
//!
//! Produces the seeded, reproducible document fixture used by later slices' benchmarks:
//! a synthetic vocabulary (lowercase ASCII words mixed with Hiragana/Katakana/Kanji
//! tokens) plus per-document term-id sequences sampled from a Zipf-ish distribution.
//! Generation is fully deterministic: one xorshift64 stream, plain IEEE-754 f64
//! arithmetic for the weight table, and no hash-map iteration anywhere.

/// Configuration for [`generate`].
pub struct CorpusConfig {
    /// Seed for the xorshift64 stream; equal seeds reproduce identical corpora.
    pub seed: u64,
    /// Number of documents (must be >= 1).
    pub docs: u32,
    /// Mean tokens per document; individual lengths jitter within ±avg_len/4 and are
    /// clamped to >= 1.
    pub avg_len: u32,
    /// Vocabulary size upper bound (must be >= 1).
    pub vocab_size: u32,
    /// Zipf skew exponent `s`: term of rank r carries weight ∝ 1/(r+1)^s.
    pub zipf_s: f64,
}

/// A generated corpus: vocabulary plus per-document term indices.
pub struct Corpus {
    /// Synthetic vocabulary; documents store indices into this vector.
    pub vocab: Vec<String>,
    /// Per-document token stream as term indices into [`Corpus::vocab`].
    pub docs: Vec<Vec<u32>>,
}

/// Golden-ratio multiplier that mixes sparse seeds before forcing the xorshift64 state
/// nonzero; from state 0 xorshift64 collapses to an all-zero stream.
const SEED_MIX: u64 = 0x9E37_79B9_7F4A_7C15;

/// xorshift64 — deterministic pseudo-random stream, same style as
/// `ic-stable-vector-page-store/src/bench.rs`. State must be nonzero.
fn next_rand(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Uniform f64 in `[0, 1)` from 53 random bits; scaling by 2^-53 is exact.
fn uniform01(state: &mut u64) -> f64 {
    ((next_rand(state) >> 11) as f64) * (1.0 / (1u64 << 53) as f64)
}

/// Draws one char from a contiguous Unicode scalar range of the given width.
fn rand_char(state: &mut u64, base: u32, width: u32) -> char {
    char::from_u32(base + (next_rand(state) % u64::from(width)) as u32)
        .expect("synthetic ranges are valid Unicode")
}

/// Lowercase ASCII word, length 3..=9.
fn ascii_word(state: &mut u64) -> String {
    let len = 3 + (next_rand(state) % 7) as usize;
    (0..len)
        .map(|_| rand_char(state, u32::from(b'a'), 26))
        .collect()
}

/// Synthetic Japanese token, length 2..=4 chars drawn from one script per token:
/// Hiragana U+3041..=U+3096, Katakana U+30A1..=U+30FA, or common Kanji U+4E00..=U+9FA5.
fn japanese_token(state: &mut u64) -> String {
    let len = 2 + (next_rand(state) % 3) as usize;
    let (base, width) = match next_rand(state) % 3 {
        0 => (0x3041, 0x0056),
        1 => (0x30A1, 0x005A),
        _ => (0x4E00, 0x51A6),
    };
    (0..len).map(|_| rand_char(state, base, width)).collect()
}

/// Even slots get ASCII words, odd slots Japanese tokens: the script mix is fixed by
/// construction rather than by random draw.
fn build_vocab(vocab_size: u32, state: &mut u64) -> Vec<String> {
    (0..vocab_size)
        .map(|i| {
            if i % 2 == 0 {
                ascii_word(state)
            } else {
                japanese_token(state)
            }
        })
        .collect()
}

/// Cumulative normalized Zipf weights: entry i holds sum(r <= i) of 1/(r+1)^s divided by
/// the total. Built once per generation; sampling binary-searches a uniform draw.
fn zipf_cumulative(vocab_size: u32, zipf_s: f64) -> Vec<f64> {
    let mut cumulative = Vec::with_capacity(vocab_size as usize);
    let mut total = 0.0;
    for rank in 0..vocab_size {
        total += (f64::from(rank) + 1.0).powf(-zipf_s);
        cumulative.push(total);
    }
    for weight in &mut cumulative {
        *weight /= total;
    }
    cumulative
}

/// Samples a vocabulary index: the first cumulative weight exceeding a uniform draw.
fn sample_index(cumulative: &[f64], state: &mut u64) -> u32 {
    let target = uniform01(state);
    // First entry > target; the clamp guards float rounding at the top of the table.
    cumulative
        .partition_point(|&weight| weight <= target)
        .min(cumulative.len() - 1) as u32
}

/// Document length: `avg_len` jittered uniformly within ±avg_len/4, clamped to >= 1.
fn doc_len(avg_len: u32, state: &mut u64) -> u32 {
    let quarter = i64::from(avg_len / 4);
    let jitter = (next_rand(state) % (2 * quarter as u64 + 1)) as i64 - quarter;
    (i64::from(avg_len) + jitter).max(1) as u32
}

/// Generates the corpus described by `config`.
///
/// # Panics
///
/// Panics when `docs`, `vocab_size`, or `avg_len` is zero: the fixture contract requires
/// at least one document, one vocabulary slot, and document length >= 1.
pub fn generate(config: CorpusConfig) -> Corpus {
    assert!(config.docs >= 1, "docs must be >= 1");
    assert!(config.vocab_size >= 1, "vocab_size must be >= 1");
    assert!(config.avg_len >= 1, "avg_len must be >= 1");
    let mut state = config.seed.wrapping_mul(SEED_MIX) | 1;
    let vocab = build_vocab(config.vocab_size, &mut state);
    let cumulative = zipf_cumulative(config.vocab_size, config.zipf_s);
    let docs = (0..config.docs)
        .map(|_| {
            let len = doc_len(config.avg_len, &mut state);
            (0..len)
                .map(|_| sample_index(&cumulative, &mut state))
                .collect::<Vec<u32>>()
        })
        .collect();
    Corpus { vocab, docs }
}

/// True when every char falls in the CJK ranges below; empty tokens are not CJK.
fn is_cjk_char(c: char) -> bool {
    matches!(
        c,
        '\u{3041}'..='\u{3096}' | '\u{30A1}'..='\u{30FF}' | '\u{4E00}'..='\u{9FFF}'
    )
}

/// Classifies synthetic tokens by script purity: true iff `token` is non-empty and every
/// character is Hiragana (U+3041..=U+3096), Katakana (U+30A1..=U+30FF), or CJK Unified
/// Ideographs (U+4E00..=U+9FFF).
pub fn is_cjk_token(token: &str) -> bool {
    !token.is_empty() && token.chars().all(is_cjk_char)
}

/// Expands one token into indexable units: overlapping character bigrams for each CJK
/// run (a lone CJK character survives as a unigram), whole segments otherwise.
///
/// `"あいうえ"` → `["あい", "いう", "うえ"]`; `"hello"` → `["hello"]`.
pub fn expand_bigrams(token: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut run_start = 0usize;
    let mut run_cjk = false;
    let mut started = false;
    for (idx, c) in token.char_indices() {
        if !started || is_cjk_char(c) != run_cjk {
            if started {
                push_run(&mut out, &token[run_start..idx], run_cjk);
            }
            run_start = idx;
            run_cjk = is_cjk_char(c);
            started = true;
        }
    }
    if started {
        push_run(&mut out, &token[run_start..], run_cjk);
    }
    out
}

/// Emits one same-script run: bigrams for CJK (unigram fallback for a single char), the
/// segment unchanged otherwise.
fn push_run(out: &mut Vec<String>, run: &str, is_cjk: bool) {
    if !is_cjk {
        out.push(run.to_string());
        return;
    }
    let chars: Vec<char> = run.chars().collect();
    match chars.as_slice() {
        [c] => out.push(c.to_string()),
        _ => {
            for pair in chars.windows(2) {
                out.push(pair.iter().collect());
            }
        }
    }
}

/// FNV-1a 64-bit (offset basis 0xcbf29ce484222325, prime 0x100000001b3). Stable across
/// Rust releases, unlike `DefaultHasher`, whose algorithm may change between versions.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid_config(seed: u64) -> CorpusConfig {
        CorpusConfig {
            seed,
            docs: 200,
            avg_len: 24,
            vocab_size: 512,
            zipf_s: 1.0,
        }
    }

    /// Length-prefixed vocab bytes followed by all doc token ids: an unambiguous
    /// canonical form so equal corpora always hash identically.
    fn canonical_bytes(corpus: &Corpus) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(corpus.vocab.len() as u32).to_le_bytes());
        for term in &corpus.vocab {
            bytes.extend_from_slice(&(term.len() as u32).to_le_bytes());
            bytes.extend_from_slice(term.as_bytes());
        }
        bytes.extend_from_slice(&(corpus.docs.len() as u32).to_le_bytes());
        for doc in &corpus.docs {
            bytes.extend_from_slice(&(doc.len() as u32).to_le_bytes());
            for id in doc {
                bytes.extend_from_slice(&id.to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn golden_hash_is_stable_for_equal_seeds_and_differs_across_seeds() {
        let first = generate(mid_config(42));
        let second = generate(mid_config(42));
        let first_hash = fnv1a64(&canonical_bytes(&first));
        assert_eq!(first_hash, fnv1a64(&canonical_bytes(&second)));
        let other_seed = generate(mid_config(43));
        assert_ne!(first_hash, fnv1a64(&canonical_bytes(&other_seed)));
    }

    #[test]
    fn mid_config_stats_hit_the_fixture_contract() {
        let corpus = generate(mid_config(7));
        assert_eq!(corpus.docs.len(), 200);
        assert!(corpus.vocab.len() <= 512);
        // The generator must mix both scripts into the vocabulary.
        assert!(corpus.vocab.iter().any(|term| is_cjk_token(term)));
        assert!(corpus.vocab.iter().any(|term| !is_cjk_token(term)));
        let total: usize = corpus.docs.iter().map(Vec::len).sum();
        let target = 4800usize; // docs * avg_len
        let tolerance = target * 15 / 100; // ±15%
        assert!(
            total.abs_diff(target) <= tolerance,
            "total tokens {total} outside ±15% of {target}"
        );
        for doc in &corpus.docs {
            assert!(!doc.is_empty());
            for id in doc {
                assert!((*id as usize) < corpus.vocab.len());
            }
        }
    }

    #[test]
    fn expand_bigrams_splits_a_cjk_run_into_overlapping_pairs() {
        assert_eq!(expand_bigrams("あいうえ"), vec!["あい", "いう", "うえ"]);
    }

    #[test]
    fn expand_bigrams_passes_non_cjk_through_unchanged() {
        assert_eq!(expand_bigrams("hello"), vec!["hello"]);
    }

    #[test]
    fn is_cjk_token_requires_script_purity() {
        assert!(is_cjk_token("ひらがな"));
        assert!(is_cjk_token("カタカナ"));
        assert!(is_cjk_token("漢字"));
        assert!(!is_cjk_token("ascii"));
        assert!(!is_cjk_token(""));
        assert!(!is_cjk_token("a日")); // mixed scripts are not a CJK token
    }
}
