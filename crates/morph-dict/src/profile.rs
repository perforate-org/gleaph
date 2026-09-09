//! DictionaryProfile — per-language unit-emission parameterization.
//!
//! MeCab-format dictionaries are language-independent STRUCTURES (double-array trie +
//! connection matrix + char def + unknown dict); what varies by language is the feature
//! string layout (which comma-separated column carries the lemma) and the unit-emission
//! policy (which 品詞1 categories are dropped from the indexable units). This struct
//! carries that policy so the engine stays language-neutral: the Japanese ipadic profile
//! is the shipped instance; Korean (mecab-ko-dic) and Chinese (jieba-converted)
//! dictionaries plug in later with a new profile and no structural changes.

/// Unit-emission policy over a MeCab-format feature string
/// `品詞,品詞細分類1,品詞細分類2,品詞細分類3,活用型,活用形,基本形,読み,発音` (ipadic layout;
/// other dictionaries may carry more/fewer columns — only the lemma column index matters).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictionaryProfile {
    /// Feature column emitted as the indexable lemma (ipadic: 6 = 基本形). `None` = emit
    /// the surface for every kept token.
    pub lemma_column: Option<usize>,
    /// Feature column 0 (品詞1) values dropped from the output (ipadic: 助詞/助動詞/記号/
    /// 接頭辞/接尾辞/フィラー).
    pub drop_categories: Vec<String>,
    /// Additive PREFIX drop set over the first '+'-separated subtag of feature column 0
    /// (plan 0341: ko-dic compound tags VV+EC, ETM+NNG+JC make exact-match unusable —
    /// 700+ combos; empty for ipadic = zero behavior change).
    pub drop_prefixes: Vec<String>,
    /// Common-prefix search bound in bytes: the trie walk never consumes more than this
    /// many bytes of the remaining input per position (MeCab-equivalent bounded lookup;
    /// also the O(n²)→O(n·k) long-line guard). Must end on a UTF-8 char boundary — the
    /// engine clamps to the last boundary at or below this limit.
    pub max_lookup_bytes: usize,
    /// Unknown-word grouping bound in bytes for `group` categories (MeCab's
    /// max-grouping-size, tightened for fail-closed lattice memory bounds).
    pub unk_max_group_bytes: usize,
    /// A single analysis line longer than this fails closed (see [`DictionaryProfile::
    /// max_line_bytes`]): the lattice/Viterbi are linear in bounded-lookup regime, but a
    /// pathological no-boundary line has no place in an index pipeline.
    pub max_line_bytes: usize,
}

impl DictionaryProfile {
    /// The shipped Japanese ipadic profile: 品詞1 = column 0 (drop set 助詞/助動詞/記号/
    /// 接頭辞/接尾辞/フィラー), 基本形 = column 6 lemma, 64-byte bounded lookup,
    /// 64-byte unk grouping, 1 MiB line bound.
    pub fn japanese_ipadic() -> Self {
        Self {
            lemma_column: Some(6),
            drop_categories: ["助詞", "助動詞", "記号", "接頭辞", "接尾辞", "フィラー"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            drop_prefixes: Vec::new(),
            max_lookup_bytes: 64,
            unk_max_group_bytes: 64,
            max_line_bytes: 1024 * 1024,
        }
    }
    /// The Korean mecab-ko-dic profile (plan 0341): feature layout
    /// `POS,semantic,jongseong,lemma,Compound/Inflect,POS2,POS3,analysis` (8 cols, no
    /// 基本形; verbs stored as stems) so `lemma_column: Some(3)`; keeps
    /// NNG/NNP/NNB/NNBC/NR/NP/VV/VA/VX/VCP/VCN/MAG/MAJ/MM/IC/XR/SL/SH (SL/SH kept:
    /// 외국어/한자 are content-bearing), drops J/E/XSN/XSV/XSA/SF/SE/SC/SSC/SSO/SP/SY/SW/
    /// UNA/NA prefixes.
    pub fn korean_mecab_ko_dic() -> Self {
        Self {
            lemma_column: Some(3),
            drop_categories: Vec::new(),
            drop_prefixes: [
                "J", "E", "XSN", "XSV", "XSA", "SF", "SE", "SC", "SSC", "SSO", "SP", "SY", "SW",
                "UNA", "NA",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            max_lookup_bytes: 64,
            unk_max_group_bytes: 64,
            max_line_bytes: 1024 * 1024,
        }
    }
}

impl DictionaryProfile {
    /// Whether the 品詞1 category is dropped under this profile: exact-match on
    /// `drop_categories` OR prefix-match of the first '+'-separated subtag against
    /// `drop_prefixes`.
    pub fn drops(&self, category: &str) -> bool {
        if self.drop_categories.iter().any(|c| c == category) {
            return true;
        }
        let subtag = category.split('+').next().unwrap_or(category);
        self.drop_prefixes.iter().any(|p| subtag.starts_with(p))
    }
}
