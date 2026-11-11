//! Tokenisation for the question-only routing prompt.
//!
//! The prompt is `[bos] + question tokens + <mask> x 6`; the six masks are the option markers the
//! canister scores. Tokenisation stays on the client: the canister has no tokenizer or text, and
//! the client keeps the byte offsets needed to turn BIO spans back into question substrings.

use std::path::Path;

use tokenizers::Tokenizer;

use super::RoutingError;

/// Default cap on question tokens; longer questions are split into windows by the canister.
pub const MAX_QUESTION_TOKENS: usize = 256;

/// One tokenised question ready for the inference canister.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenizedQuestion {
    /// Question token ids; the canister prepends `<bos>` and appends the `<mask>` markers.
    pub ids: Vec<u32>,
    /// Byte ranges of the question tokens in the original text; `offsets[i]` belongs to
    /// `ids[i]`.
    pub offsets: Vec<(usize, usize)>,
    /// True when the question exceeded [`MAX_QUESTION_TOKENS`] and was truncated.
    pub truncated: bool,
}

/// Tokenizer plus the question-token cap of the routing model.
pub struct RoutingTokenizer {
    tokenizer: Tokenizer,
    max_tokens: usize,
}

impl RoutingTokenizer {
    /// Load a `tokenizer.json` and resolve the `<bos>`/`<mask>` ids from it.
    pub fn from_file(path: &Path) -> Result<Self, RoutingError> {
        Self::from_file_with_limit(path, MAX_QUESTION_TOKENS)
    }

    /// Load a `tokenizer.json` with an explicit question-token cap.
    pub fn from_file_with_limit(path: &Path, max_tokens: usize) -> Result<Self, RoutingError> {
        let mut tokenizer = Tokenizer::from_file(path)
            .map_err(|error| RoutingError::Tokenizer(error.to_string()))?;
        // The model's tokenizer.json carries a 48-token truncation; the SDK owns the cap and the
        // canister owns chunking, so the tokenizer must not truncate on its own.
        tokenizer
            .with_truncation(None)
            .map_err(|error| RoutingError::Tokenizer(error.to_string()))?;
        Ok(Self {
            tokenizer,
            max_tokens,
        })
    }

    /// Fraction of CJK characters in the decoded token stream (0.0 when decoding fails).
    pub fn cjk_fraction(&self, ids: &[u32]) -> f32 {
        let Ok(text) = self.tokenizer.decode(ids, true) else {
            return 0.0;
        };
        let mut total = 0usize;
        let mut cjk = 0usize;
        for character in text.chars() {
            if character.is_whitespace() {
                continue;
            }
            total += 1;
            let code = character as u32;
            let is_cjk = (0x3040..=0x30ff).contains(&code)
                || (0x3400..=0x4dbf).contains(&code)
                || (0x4e00..=0x9fff).contains(&code)
                || (0xf900..=0xfaff).contains(&code)
                || (0xff66..=0xff9f).contains(&code);
            cjk += usize::from(is_cjk);
        }
        if total == 0 {
            0.0
        } else {
            cjk as f32 / total as f32
        }
    }

    /// Tokenise a question into the canister's prompt layout.
    pub fn tokenize(&self, question: &str) -> Result<TokenizedQuestion, RoutingError> {
        let encoding = self
            .tokenizer
            .encode(question, false)
            .map_err(|error| RoutingError::Tokenizer(error.to_string()))?;
        let mut offsets: Vec<(usize, usize)> = encoding.get_offsets().to_vec();
        let mut tokens: Vec<u32> = encoding.get_ids().to_vec();
        let truncated = tokens.len() > self.max_tokens;
        tokens.truncate(self.max_tokens);
        offsets.truncate(self.max_tokens);
        Ok(TokenizedQuestion {
            ids: tokens,
            offsets,
            truncated,
        })
    }
}

impl std::fmt::Debug for RoutingTokenizer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoutingTokenizer")
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenizer() -> Option<RoutingTokenizer> {
        let path = std::env::var_os("GLEAPH_TOKENIZER")?;
        RoutingTokenizer::from_file(Path::new(&path)).ok()
    }

    #[test]
    fn question_tokens_match_the_canonical_tokenizer() {
        let Some(tokenizer) = tokenizer() else {
            return;
        };
        let question = "Which concepts reach Graph databases within 2 hops?";
        let tokenized = tokenizer.tokenize(question).expect("tokenize");
        let expected = vec![
            12236, 17482, 6378, 14121, 45084, 2819, 235248, 235284, 96211, 235336,
        ];
        assert!(!tokenized.truncated);
        assert_eq!(tokenized.ids, expected);
        assert_eq!(tokenized.offsets.len(), expected.len());
        for (start, end) in &tokenized.offsets {
            assert!(start < end && *end <= question.len());
        }
    }

    #[test]
    fn long_questions_are_truncated_and_flagged() {
        let Some(tokenizer) = tokenizer() else {
            return;
        };
        let question = "concept ".repeat(400);
        let tokenized = tokenizer.tokenize(&question).expect("tokenize");
        assert!(tokenized.truncated);
        assert_eq!(tokenized.ids.len(), MAX_QUESTION_TOKENS);
        assert_eq!(tokenized.offsets.len(), MAX_QUESTION_TOKENS);
    }

    #[test]
    fn japanese_offsets_stay_byte_ranges() {
        let Some(tokenizer) = tokenizer() else {
            return;
        };
        let question = "GraphRAG を本文に含む文書を検索して";
        let tokenized = tokenizer.tokenize(question).expect("tokenize");
        assert_eq!(tokenized.offsets[0], (0, 5));
        assert_eq!(tokenized.offsets[1], (5, 8));
        // Offsets are byte ranges (UTF-8), so slicing the question stays valid.
        let (start, end) = *tokenized.offsets.last().unwrap();
        assert_eq!(&question[start..end], "して");
    }
}
